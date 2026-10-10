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
}
