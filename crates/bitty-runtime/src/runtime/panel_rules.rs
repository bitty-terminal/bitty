//! Declarative panel spawn-rule evaluation (CTX-1080, issue 1756).
//!
//! Pure helpers over `bitty-config` rules plus the minimal `Runtime` hook.
//! Matchers read only already-available spawn metadata (program plus argv,
//! current title, content kind); no new process or environment authority is
//! introduced. First match wins in array order; invalid patterns never
//! match.

use bitty_config::panel_rules::{PanelSpawnRule, command_line_for, find_match};

/// Spawn metadata already available at PTY spawn time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnMetadata {
    /// Full command line (`program` plus `args` joined with spaces).
    pub cmd_line: String,
    /// Current title text (empty at spawn; re-evaluated on title change).
    pub title: String,
    /// Content kind (`terminal` for spawned panes).
    pub content: String,
}

impl SpawnMetadata {
    /// Builds metadata from spawn arguments; title starts empty.
    #[must_use]
    pub fn for_spawn(program: &str, args: &[&str]) -> Self {
        Self {
            cmd_line: command_line_for(program, args),
            title: String::new(),
            content: "terminal".to_string(),
        }
    }
}

/// Evaluates `rules` against `metadata`, returning the first match.
#[must_use]
pub fn evaluate_panel_rules<'a>(
    rules: &'a [PanelSpawnRule],
    metadata: &SpawnMetadata,
) -> Option<(usize, &'a PanelSpawnRule)> {
    find_match(
        rules,
        &metadata.cmd_line,
        &metadata.title,
        &metadata.content,
    )
}

/// Maps a config presentation to the UI presentation mode.
#[must_use]
pub fn presentation_for(rule: &PanelSpawnRule) -> Option<bitty_ui::presentation::PresentationMode> {
    match rule.presentation {
        Some(bitty_config::panel_rules::PanelPresentation::Tiled) => {
            Some(bitty_ui::presentation::PresentationMode::Tiled)
        }
        Some(bitty_config::panel_rules::PanelPresentation::Floating) => {
            Some(bitty_ui::presentation::PresentationMode::Floating)
        }
        Some(bitty_config::panel_rules::PanelPresentation::Scratchpad) => {
            Some(bitty_ui::presentation::PresentationMode::Scratchpad)
        }
        None => None,
    }
}

/// Requested initial dimensions from a matched rule, if any.
#[must_use]
pub fn dims_for(rule: &PanelSpawnRule) -> (Option<u16>, Option<u16>) {
    (rule.width, rule.height)
}

/// Requested workspace label from a matched rule, if any.
#[must_use]
pub fn workspace_for(rule: &PanelSpawnRule) -> Option<u8> {
    rule.workspace
}

/// Whether the rule requests centering.
#[must_use]
pub fn centered_for(rule: &PanelSpawnRule) -> bool {
    rule.centered.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_config::panel_rules::{PanelPresentation, PanelSpawnRule};
    // Only the POSIX-shell live-spawn tests below use this (all
    // `#[cfg(unix)]`); without the gate the import is unused on Windows.
    #[cfg(unix)]
    use bitty_test_support::require_pty;

    fn floating_btop() -> PanelSpawnRule {
        PanelSpawnRule {
            cmd: Some("btop".to_string()),
            cmd_regex: None,
            title_regex: None,
            content: None,
            presentation: Some(PanelPresentation::Floating),
            width: Some(100),
            height: Some(30),
            workspace: None,
            centered: Some(true),
        }
    }

    #[test]
    fn btop_rule_matches_and_maps() {
        let rules = vec![floating_btop()];
        let meta = SpawnMetadata::for_spawn("btop", &[]);
        let (_, hit) = evaluate_panel_rules(&rules, &meta).expect("match");
        assert_eq!(
            presentation_for(hit),
            Some(bitty_ui::presentation::PresentationMode::Floating)
        );
        assert_eq!(dims_for(hit), (Some(100), Some(30)));
        assert!(centered_for(hit));
        assert_eq!(workspace_for(hit), None);
    }

    #[test]
    fn tail_rule_assigns_workspace() {
        let rules = vec![PanelSpawnRule {
            cmd: None,
            cmd_regex: Some("^tail -f".to_string()),
            title_regex: None,
            content: None,
            presentation: None,
            width: None,
            height: None,
            workspace: Some(3),
            centered: None,
        }];
        let meta = SpawnMetadata::for_spawn("tail", &["-f", "/var/log/syslog"]);
        let (_, hit) = evaluate_panel_rules(&rules, &meta).expect("match");
        assert_eq!(workspace_for(hit), Some(3));
    }

    #[test]
    fn ordering_first_wins() {
        let first = floating_btop();
        let second = PanelSpawnRule {
            cmd: Some("btop".to_string()),
            cmd_regex: None,
            title_regex: None,
            content: None,
            presentation: Some(PanelPresentation::Tiled),
            width: None,
            height: None,
            workspace: None,
            centered: None,
        };
        let rules = vec![first, second];
        let meta = SpawnMetadata::for_spawn("/usr/bin/btop", &[]);
        let (idx, _) = evaluate_panel_rules(&rules, &meta).expect("match");
        assert_eq!(idx, 0);
    }

    #[test]
    fn invalid_rule_never_matches() {
        let rules = vec![PanelSpawnRule {
            cmd: None,
            cmd_regex: Some("([".to_string()),
            title_regex: None,
            content: None,
            presentation: Some(PanelPresentation::Floating),
            width: None,
            height: None,
            workspace: None,
            centered: None,
        }];
        let meta = SpawnMetadata::for_spawn("([", &[]);
        assert!(evaluate_panel_rules(&rules, &meta).is_none());
    }

    #[test]
    fn no_match_returns_none() {
        let rules = vec![floating_btop()];
        let meta = SpawnMetadata::for_spawn("htop", &[]);
        assert!(evaluate_panel_rules(&rules, &meta).is_none());
    }

    // Live-spawn: runs a real POSIX shell (`/bin/sh` has no Windows
    // equivalent). `#[cfg(unix)]` keeps it off Windows CI; `require_pty!()`
    // keeps the force-no-PTY simulation path (workspaces.rs precedent).
    #[test]
    #[cfg(unix)]
    fn spawn_applies_presentation_and_dims() {
        require_pty!();
        use crate::Runtime;
        let mut rt = Runtime::with_defaults().expect("defaults build");
        rt.set_panel_spawn_rules(vec![PanelSpawnRule {
            cmd: Some("sh".to_string()),
            cmd_regex: None,
            title_regex: None,
            content: None,
            presentation: Some(PanelPresentation::Floating),
            width: Some(100),
            height: Some(30),
            workspace: None,
            centered: Some(true),
        }]);
        let focused = rt.focused_view().expect("focused leaf");
        rt.spawn_shell_for_view(focused, "/bin/sh", &[], 40, 12)
            .expect("spawn");
        let leaf = rt.layout().find_leaf(focused).expect("leaf present");
        assert_eq!(
            leaf.presentation(),
            bitty_ui::presentation::PresentationMode::Floating
        );
        assert_eq!(leaf.cols(), 100);
        assert_eq!(leaf.rows(), 30);
    }

    #[test]
    #[cfg(unix)]
    fn spawn_assigns_workspace_when_focused() {
        require_pty!();
        use crate::Runtime;
        use bitty_ui::{LayoutNode, View, ViewId};
        let mut rt = Runtime::with_defaults().expect("defaults build");
        let first = rt.focused_view().expect("focused leaf");
        let second = ViewId::new(2);
        let old = rt.layout().find_leaf(first).expect("leaf").clone();
        rt.set_layout(LayoutNode::split(
            bitty_ui::SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(old),
            LayoutNode::leaf(View::new(second, 40, 12)),
        ));
        assert!(rt.set_focus(first));
        rt.workspace_new().expect("ws2");
        assert!(rt.workspace_switch(0));
        assert_eq!(rt.focused_view(), Some(first));
        rt.set_panel_spawn_rules(vec![PanelSpawnRule {
            cmd: None,
            cmd_regex: Some("^/bin/sh".to_string()),
            title_regex: None,
            content: None,
            presentation: None,
            width: None,
            height: None,
            workspace: Some(2),
            centered: None,
        }]);
        rt.spawn_shell_for_view(first, "/bin/sh", &[], 40, 12)
            .expect("spawn");
        assert_eq!(rt.active_workspace_index(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn empty_rules_leave_spawn_unchanged() {
        require_pty!();
        use crate::Runtime;
        let mut rt = Runtime::with_defaults().expect("defaults build");
        assert!(rt.panel_spawn_rules().is_empty());
        let focused = rt.focused_view().expect("focused leaf");
        rt.spawn_shell_for_view(focused, "/bin/sh", &[], 40, 12)
            .expect("spawn");
        let leaf = rt.layout().find_leaf(focused).expect("leaf present");
        assert_eq!(
            leaf.presentation(),
            bitty_ui::presentation::PresentationMode::Tiled
        );
    }

    #[test]
    #[cfg(unix)]
    fn primary_spawn_applies_matching_rule() {
        require_pty!();
        use crate::Runtime;
        let mut rt = Runtime::with_defaults().expect("defaults build");
        rt.set_panel_spawn_rules(vec![PanelSpawnRule {
            cmd: Some("sh".to_string()),
            cmd_regex: None,
            title_regex: None,
            content: None,
            presentation: Some(PanelPresentation::Floating),
            width: Some(100),
            height: Some(30),
            workspace: None,
            centered: None,
        }]);
        rt.spawn_shell_with_args("/bin/sh", &[])
            .expect("primary spawn");
        assert_eq!(rt.pty_size(), Some((100, 30)));
        let owner = rt.primary_view().expect("primary owner");
        let leaf = rt.layout().find_leaf(owner).expect("leaf present");
        assert_eq!(
            leaf.presentation(),
            bitty_ui::presentation::PresentationMode::Floating
        );
        assert_eq!(leaf.cols(), 100);
        assert_eq!(leaf.rows(), 30);
        assert_eq!(rt.state().width(), 100);
        assert_eq!(rt.state().height(), 30);
    }

    #[test]
    #[cfg(unix)]
    fn rule_dims_are_durable_through_solver_sync() {
        require_pty!();
        use crate::Runtime;
        let mut rt = Runtime::with_defaults().expect("defaults build");
        rt.set_panel_spawn_rules(vec![PanelSpawnRule {
            cmd: Some("sh".to_string()),
            cmd_regex: None,
            title_regex: None,
            content: None,
            presentation: None,
            width: Some(100),
            height: Some(30),
            workspace: None,
            centered: None,
        }]);
        let focused = rt.focused_view().expect("focused leaf");
        rt.spawn_shell_for_view(focused, "/bin/sh", &[], 40, 12)
            .expect("spawn");
        // Immediate: the rule sizes the PTY at spawn and stamps the flag.
        assert_eq!(rt.pane_pty_size(&focused), Some((100, 30)));
        let leaf = rt.layout().find_leaf(focused).expect("leaf present");
        assert_eq!(leaf.fixed_size(), Some(crate::UiSize::new(100, 30)));
        // Steady-state split: paint stays slot-sized (the default 80x24
        // container cannot fit 100x30) while geometry sync keeps the grid
        // and PTY at the rule size instead of reflowing to the solver
        // frame.
        let frames = rt.present_frames();
        let frame = frames.iter().find(|f| f.view == focused).expect("frame");
        assert_ne!((frame.cols, frame.rows), (100, 30));
        rt.sync_pane_geometry_to(&frames);
        assert_eq!(rt.pane_pty_size(&focused), Some((100, 30)));
        let grid = rt.pane_snapshot(&focused).expect("pane grid");
        assert_eq!((grid.width, grid.height), (100, 30));
        // Reflow keeps the leaf at the solver slot for the paint paths.
        rt.reflow_present_layout(&frames);
        let leaf = rt.layout().find_leaf(focused).expect("leaf present");
        assert_eq!((leaf.cols(), leaf.rows()), (frame.cols, frame.rows));
        assert_eq!(leaf.fixed_size(), Some(crate::UiSize::new(100, 30)));
    }

    #[test]
    #[cfg(unix)]
    fn respawn_preserves_existing_fixed_size() {
        require_pty!();
        use crate::Runtime;
        let mut rt = Runtime::with_defaults().expect("defaults build");
        let rule = |w: u16, h: u16| PanelSpawnRule {
            cmd: Some("sh".to_string()),
            cmd_regex: None,
            title_regex: None,
            content: None,
            presentation: None,
            width: Some(w),
            height: Some(h),
            workspace: None,
            centered: None,
        };
        rt.set_panel_spawn_rules(vec![rule(100, 30)]);
        let focused = rt.focused_view().expect("focused leaf");
        rt.spawn_shell_for_view(focused, "/bin/sh", &[], 40, 12)
            .expect("spawn");
        assert_eq!(
            rt.layout().find_leaf(focused).expect("leaf").fixed_size(),
            Some(crate::UiSize::new(100, 30))
        );
        // A rule edit plus respawn must not overwrite the kept flag: the
        // first stamp wins until explicitly cleared.
        rt.set_panel_spawn_rules(vec![rule(50, 20)]);
        rt.spawn_shell_for_view(focused, "/bin/sh", &[], 40, 12)
            .expect("respawn");
        assert_eq!(
            rt.layout().find_leaf(focused).expect("leaf").fixed_size(),
            Some(crate::UiSize::new(100, 30))
        );
        let frames = rt.present_frames();
        rt.sync_pane_geometry_to(&frames);
        assert_eq!(rt.pane_pty_size(&focused), Some((100, 30)));
        let grid = rt.pane_snapshot(&focused).expect("pane grid");
        assert_eq!((grid.width, grid.height), (100, 30));
    }

    #[test]
    #[cfg(unix)]
    fn clear_fixed_returns_to_solver_ownership() {
        require_pty!();
        use crate::Runtime;
        let mut rt = Runtime::with_defaults().expect("defaults build");
        rt.set_panel_spawn_rules(vec![PanelSpawnRule {
            cmd: Some("sh".to_string()),
            cmd_regex: None,
            title_regex: None,
            content: None,
            presentation: None,
            width: Some(100),
            height: Some(30),
            workspace: None,
            centered: None,
        }]);
        let focused = rt.focused_view().expect("focused leaf");
        rt.spawn_shell_for_view(focused, "/bin/sh", &[], 40, 12)
            .expect("spawn");
        assert_eq!(rt.pane_pty_size(&focused), Some((100, 30)));
        // Clearing the per-view flag returns the view to solver ownership
        // on the next sync: grid and PTY follow the solver frame again.
        let mut tree = rt.layout().clone();
        tree.find_leaf_mut(focused)
            .expect("leaf present")
            .clear_fixed_size();
        rt.set_layout(tree);
        let frames = rt.present_frames();
        let frame = frames.iter().find(|f| f.view == focused).expect("frame");
        assert_ne!((frame.cols, frame.rows), (100, 30));
        rt.sync_pane_geometry_to(&frames);
        assert_eq!(rt.pane_pty_size(&focused), Some((frame.cols, frame.rows)));
    }

    #[test]
    fn degenerate_fixed_dims_fail_closed_headless() {
        use crate::Runtime;
        use bitty_ui::{LayoutNode, View, ViewId};
        let mut rt = Runtime::with_defaults().expect("defaults build");
        let id = ViewId::new(1);
        // Degenerate stamps fail closed to solver ownership (stored None).
        let mut tree = LayoutNode::leaf(View::new(id, 80, 24));
        tree.find_leaf_mut(id)
            .expect("leaf")
            .set_fixed_size(Some(crate::UiSize::new(0, 24)));
        assert_eq!(tree.find_leaf(id).expect("leaf").fixed_size(), None);
        tree.find_leaf_mut(id)
            .expect("leaf")
            .set_fixed_size(Some(crate::UiSize::new(2000, 2000)));
        assert_eq!(tree.find_leaf(id).expect("leaf").fixed_size(), None);
        // A valid stamp survives solver reflow and present; clearing
        // returns to the solver frame.
        tree.find_leaf_mut(id)
            .expect("leaf")
            .set_fixed_size(Some(crate::UiSize::new(40, 12)));
        rt.set_layout(tree);
        rt.set_container(crate::UiRect::new(0, 0, 100, 40));
        let frame = rt
            .present_frames()
            .into_iter()
            .find(|frame| frame.view == id)
            .expect("frame");
        assert_eq!((frame.cols, frame.rows), (40, 12));
        let leaf = rt.layout().find_leaf(id).expect("leaf");
        assert_eq!((leaf.cols(), leaf.rows()), (40, 12));
        // Direct solver reflow owns the leaf size (paint stays slot-sized
        // for the scrolled-viewport and scrollbar paths); the flag itself
        // survives for the grid/PTY sync paths.
        rt.reflow_layout();
        let leaf = rt.layout().find_leaf(id).expect("leaf");
        assert_eq!((leaf.cols(), leaf.rows()), (100, 40));
        assert_eq!(leaf.fixed_size(), Some(crate::UiSize::new(40, 12)));
        // Clearing restores solver geometry on the next layout install.
        let mut tree = rt.layout().clone();
        tree.find_leaf_mut(id).expect("leaf").clear_fixed_size();
        rt.set_layout(tree);
        let frame = rt
            .present_frames()
            .into_iter()
            .find(|frame| frame.view == id)
            .expect("frame");
        assert_ne!((frame.cols, frame.rows), (40, 12));
    }

    #[test]
    fn fixed_wins_over_pseudo_and_restores_it_headless() {
        use crate::Runtime;
        use bitty_ui::{LayoutNode, View, ViewId};
        let mut rt = Runtime::with_defaults().expect("defaults build");
        let id = ViewId::new(1);
        let mut tree = LayoutNode::leaf(View::new(id, 80, 24));
        bitty_ui::set_pseudo_size(&mut tree, id, 60, 20).expect("pseudo stamp");
        tree.find_leaf_mut(id)
            .expect("leaf")
            .set_fixed_size(Some(crate::UiSize::new(40, 12)));
        rt.set_layout(tree);
        rt.set_container(crate::UiRect::new(0, 0, 100, 40));
        // Fixed governs present while both flags are set.
        let frame = rt
            .present_frames()
            .into_iter()
            .find(|frame| frame.view == id)
            .expect("frame");
        assert_eq!((frame.cols, frame.rows), (40, 12));
        // Clearing fixed restores pseudo (which fits here).
        let mut tree = rt.layout().clone();
        tree.find_leaf_mut(id).expect("leaf").clear_fixed_size();
        rt.set_layout(tree);
        let frame = rt
            .present_frames()
            .into_iter()
            .find(|frame| frame.view == id)
            .expect("frame");
        assert_eq!((frame.cols, frame.rows), (60, 20));
    }

    #[test]
    fn fixed_floating_sizes_the_float_headless() {
        use crate::Runtime;
        use bitty_ui::{LayoutNode, PresentationMode, View, ViewId};
        let mut rt = Runtime::with_defaults().expect("defaults build");
        let id = ViewId::new(1);
        let mut tree = LayoutNode::leaf(View::new(id, 80, 24));
        tree.find_leaf_mut(id)
            .expect("leaf")
            .set_presentation(PresentationMode::Floating);
        tree.find_leaf_mut(id)
            .expect("leaf")
            .set_fixed_size(Some(crate::UiSize::new(40, 12)));
        rt.set_layout(tree);
        rt.set_container(crate::UiRect::new(0, 0, 100, 40));
        let frame = rt
            .present_frames()
            .into_iter()
            .find(|frame| frame.view == id)
            .expect("frame");
        // Floating lifts to the Float tier with the fixed grid dims.
        assert!(frame.tier.is_some());
        assert_eq!((frame.cols, frame.rows), (40, 12));
    }

    #[test]
    #[cfg(unix)]
    fn pinned_fixed_keeps_grid_through_sync() {
        require_pty!();
        use crate::Runtime;
        use bitty_ui::{LayoutNode, SplitAxis, View, ViewId};
        let mut rt = Runtime::with_defaults().expect("defaults build");
        // Two leaves: pinning refuses a stranded single-leaf layout.
        let first = rt.focused_view().expect("focused leaf");
        let second = ViewId::new(2);
        let old = rt.layout().find_leaf(first).expect("leaf").clone();
        rt.set_layout(LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(old),
            LayoutNode::leaf(View::new(second, 40, 12)),
        ));
        rt.set_panel_spawn_rules(vec![PanelSpawnRule {
            cmd: Some("sh".to_string()),
            cmd_regex: None,
            title_regex: None,
            content: None,
            presentation: None,
            width: Some(100),
            height: Some(30),
            workspace: None,
            centered: None,
        }]);
        rt.spawn_shell_for_view(first, "/bin/sh", &[], 40, 12)
            .expect("spawn");
        assert_eq!(rt.pane_pty_size(&first), Some((100, 30)));
        // Pinning moves the whole flagged view out of the layout; the
        // session survives the move.
        rt.pin_floating(first).expect("pin");
        assert!(rt.layout().find_leaf(first).is_none());
        // The pinned frame paints the anchor, not the flag — but the sync
        // must still size the grid and PTY from the pinned store flag.
        let frames = rt.present_frames();
        let frame = frames.iter().find(|f| f.view == first).expect("frame");
        assert_ne!((frame.cols, frame.rows), (100, 30));
        rt.sync_pane_geometry_to(&frames);
        assert_eq!(rt.pane_pty_size(&first), Some((100, 30)));
        let grid = rt.pane_snapshot(&first).expect("pane grid");
        assert_eq!((grid.width, grid.height), (100, 30));
    }

    #[test]
    fn explicit_zero_dims_stamp_nothing_headless() {
        use crate::Runtime;
        let mut rt = Runtime::with_defaults().expect("defaults build");
        let focused = rt.focused_view().expect("focused leaf");
        let rule = |w: Option<u16>, h: Option<u16>| PanelSpawnRule {
            cmd: Some("sh".to_string()),
            cmd_regex: None,
            title_regex: None,
            content: None,
            presentation: None,
            width: w,
            height: h,
            workspace: None,
            centered: None,
        };
        // Programmatic rules bypass config validation, so an explicit zero
        // axis must fail closed here rather than clamp to 1 as a durable
        // size.
        rt.stamp_rule_fixed_size(focused, &rule(Some(0), Some(30)), 40, 12);
        assert_eq!(
            rt.layout().find_leaf(focused).expect("leaf").fixed_size(),
            None
        );
        rt.stamp_rule_fixed_size(focused, &rule(Some(40), Some(0)), 40, 12);
        assert_eq!(
            rt.layout().find_leaf(focused).expect("leaf").fixed_size(),
            None
        );
        // An absent axis keeps the spawn fallback; a first stamp wins over
        // later ones until explicitly cleared.
        rt.stamp_rule_fixed_size(focused, &rule(None, Some(30)), 40, 12);
        assert_eq!(
            rt.layout().find_leaf(focused).expect("leaf").fixed_size(),
            Some(crate::UiSize::new(40, 30))
        );
        rt.stamp_rule_fixed_size(focused, &rule(Some(50), Some(20)), 40, 12);
        assert_eq!(
            rt.layout().find_leaf(focused).expect("leaf").fixed_size(),
            Some(crate::UiSize::new(40, 30))
        );
    }

    #[test]
    fn oversized_fixed_tiled_paints_within_slot_bounds_headless() {
        use crate::Runtime;
        use bitty_ui::{LayoutNode, SplitAxis, View, ViewId};
        fn two_pane() -> LayoutNode {
            LayoutNode::split(
                SplitAxis::Horizontal,
                0.5,
                LayoutNode::leaf(View::new(ViewId::new(1), 80, 24)),
                LayoutNode::leaf(View::new(ViewId::new(2), 80, 24)),
            )
        }
        // Baseline without any flag: the solver slots both leaves.
        let mut rt = Runtime::with_defaults().expect("defaults build");
        rt.set_layout(two_pane());
        rt.set_container(crate::UiRect::new(0, 0, 120, 40));
        let mut baseline = rt.present_frames();
        baseline.sort_by_key(|frame| frame.view);
        // An oversized fixed flag on leaf 1 must not move paint: its frame
        // stays the slot and the sibling stays byte-identical, so no grid
        // cell can bleed into the neighbouring tile.
        let mut stamped = two_pane();
        stamped
            .find_leaf_mut(ViewId::new(1))
            .expect("leaf")
            .set_fixed_size(Some(crate::UiSize::new(100, 30)));
        rt.set_layout(stamped);
        let mut after = rt.present_frames();
        after.sort_by_key(|frame| frame.view);
        assert_eq!(after.len(), baseline.len());
        for (before, now) in baseline.iter().zip(after.iter()) {
            assert_eq!(now.view, before.view);
            assert_eq!(now.frame, before.frame);
            assert_eq!(now.content, before.content);
            assert_eq!((now.cols, now.rows), (before.cols, before.rows));
            assert_eq!(now.border, before.border);
            assert_eq!(now.tier, before.tier);
        }
        // The flag itself survives the layout install for the sync paths.
        assert_eq!(
            rt.layout()
                .find_leaf(ViewId::new(1))
                .expect("leaf")
                .fixed_size(),
            Some(crate::UiSize::new(100, 30))
        );
    }
}
