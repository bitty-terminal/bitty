//! CLI grammar model shared by shell completion and shell integration
//! (issue #2372).
//!
//! The completion scripts used to carry their own hand-maintained token
//! lists, so `bitty config <TAB>` and friends offered nothing but top-level
//! words. This module builds one tree from the parser-owned tables
//! ([`crate::cli::SUBCOMMANDS`], [`ConfigCommand::VERBS`],
//! [`PluginVerb::VERBS`], [`ComponentVerb::VERBS`], [`crate::ctl::request::
//! CTL_RESOURCES`], the dev/inspect/list grammars, and
//! [`CompletionShell::all`]) and every per-shell emitter renders that tree.
//! A parser table change therefore reaches all five shells without a second
//! list to keep in sync.
//!
//! The grammar is descriptive: the parser stays the executable authority.
//! Unit tests assert every emitted node/name is reachable from the parser's
//! own tables (and that the hidden `component` alias never leaks).

use crate::cli::{ConfigCommand, SUBCOMMANDS};
use crate::completion::CompletionShell;
use crate::component::ComponentVerb;
use crate::ctl::request::CTL_RESOURCES;
use crate::dev::{
    DEV_DUMP_SUBVERBS, DEV_OVERLAY_SUBVERBS, DEV_TRACE_SUBVERBS, DEV_VERBS, OverlayName,
};
use crate::inspect::InspectTarget;
use crate::list::ListKind;
use crate::plugin::{ConfigAction, PluginVerb};
use crate::profile::ProfileVerb;

/// Global flags accepted before/after every first-level word.
pub(crate) const GLOBAL_FLAGS: &[&str] = &[
    "--help",
    "--version",
    "--verbose",
    "--log-level",
    "--headless",
    "--test-mode",
    "--safe",
    "--fail-loud",
    "--no-splash",
    "--split",
    "--split-ratio",
    "--stack",
    "--overlay",
    "--layout",
    "--focus",
    "--config",
    "--profile",
    "--theme",
    "--font-family",
    "--font-size",
    "--opacity",
    "--format",
    "--socket",
    "--instance",
    "--no-color",
    "--yes",
    "--force",
    "--scrollback",
    "--close-confirm",
    "--gaps-in",
    "--gaps-out",
    "--border",
    "--radius",
    "--plugins",
    "--no-plugins",
    "--with-components",
    "--no-with-components",
    "--no-components",
    "--without-components",
    "--print-shell-init",
    "--no-print-shell-init",
    "--no-shell-init",
];

/// Global flags that consume the following token as their value. Completion
/// uses this to skip values when resolving the subcommand path; `--split`
/// stays out because its value is optional.
pub(crate) const GLOBAL_VALUE_FLAGS: &[&str] = &[
    "--log-level",
    "--split-ratio",
    "--layout",
    "--focus",
    "--config",
    "--profile",
    "--theme",
    "--font-family",
    "--font-size",
    "--opacity",
    "--format",
    "--socket",
    "--instance",
    "--scrollback",
    "--close-confirm",
    "--gaps-in",
    "--gaps-out",
    "--border",
    "--radius",
    "--plugins",
];

/// Subcommand-specific value flags (union used by path resolution).
const NODE_VALUE_FLAGS: &[&str] = &[
    "--cwd",
    "--env",
    "--title",
    "--scope",
    "--terminal",
    "--family",
    "--cap",
    "--iterations",
    "--rows",
    "--cols",
    "--version",
    "--digest",
    "--distro",
];

/// One grammar node: a resolved subcommand path plus the words it offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Node {
    /// Space-joined path (`""` is the root, `bitty` itself).
    pub path: String,
    /// Child words and flags offered at this path (globals included).
    pub candidates: Vec<&'static str>,
    /// Whether an arbitrary operand may follow (free-form command tails).
    pub freeform: bool,
}

impl Node {
    /// Whether `candidate` is offered at this node.
    #[cfg(test)]
    pub fn offers(&self, candidate: &str) -> bool {
        self.candidates.contains(&candidate)
    }
}

fn merged_flags(extra: &[&'static str]) -> Vec<&'static str> {
    let mut flags = Vec::with_capacity(extra.len() + GLOBAL_FLAGS.len());
    for flag in extra.iter().chain(GLOBAL_FLAGS.iter()) {
        if !flags.contains(flag) {
            flags.push(*flag);
        }
    }
    flags
}

fn push_node(
    nodes: &mut Vec<Node>,
    path: &str,
    names: Vec<&'static str>,
    extra_flags: &[&'static str],
    freeform: bool,
) {
    let mut candidates = names;
    for flag in merged_flags(extra_flags) {
        if !candidates.contains(&flag) {
            candidates.push(flag);
        }
    }
    nodes.push(Node {
        path: path.to_string(),
        candidates,
        freeform,
    });
}

fn child(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix} {name}")
    }
}

fn names(values: &[&'static str]) -> Vec<&'static str> {
    values.to_vec()
}

/// Build the complete grammar tree in display order.
pub(crate) fn nodes() -> Vec<Node> {
    let mut nodes = Vec::new();

    let mut top: Vec<&'static str> = Vec::new();
    for (name, aliases) in SUBCOMMANDS {
        top.push(name);
        top.extend_from_slice(aliases);
    }
    push_node(&mut nodes, "", top.clone(), &[], false);

    push_node(
        &mut nodes,
        "run",
        Vec::new(),
        &["--cwd", "--env", "--title"],
        true,
    );

    let ctl_resources: Vec<&'static str> = CTL_RESOURCES.iter().map(|(name, _)| *name).collect();
    push_node(
        &mut nodes,
        "ctl",
        ctl_resources,
        &[
            "--socket",
            "--instance",
            "--format",
            "--no-color",
            "--cwd",
            "--scope",
            "--terminal",
            "--family",
            "--left",
            "--right",
            "--up",
            "--down",
        ],
        false,
    );
    for (resource, verbs) in CTL_RESOURCES {
        push_node(
            &mut nodes,
            &child("ctl", resource),
            verbs.to_vec(),
            &[],
            false,
        );
    }

    let config_verbs: Vec<&'static str> =
        ConfigCommand::VERBS.iter().map(|(name, _)| *name).collect();
    push_node(&mut nodes, "config", config_verbs, &[], false);
    let profile_verbs: Vec<&'static str> =
        ProfileVerb::VERBS.iter().map(|(name, _)| *name).collect();
    push_node(&mut nodes, "config profile", profile_verbs, &[], false);

    push_node(
        &mut nodes,
        "init",
        Vec::new(),
        &[
            "--yes",
            "--force",
            "--config",
            "--profile",
            "--theme",
            "--font-family",
            "--font-size",
            "--scrollback",
            "--close-confirm",
            "--gaps-in",
            "--gaps-out",
            "--border",
            "--radius",
            "--plugins",
            "--no-plugins",
            "--with-components",
            "--no-with-components",
            "--print-shell-init",
            "--no-print-shell-init",
        ],
        false,
    );

    push_node(
        &mut nodes,
        "doctor",
        Vec::new(),
        &["--format", "--no-color"],
        false,
    );
    push_node(
        &mut nodes,
        "list",
        names(ListKind::NAMES),
        &["--format", "--socket", "--instance", "--no-color"],
        false,
    );
    push_node(
        &mut nodes,
        "inspect",
        names(InspectTarget::NAMES),
        &["--format", "--no-color"],
        false,
    );

    push_node(
        &mut nodes,
        "dev",
        names(DEV_VERBS),
        &[
            "--format",
            "--no-color",
            "--iterations",
            "--rows",
            "--cols",
            "--layout",
        ],
        false,
    );
    push_node(
        &mut nodes,
        "dev trace",
        names(DEV_TRACE_SUBVERBS),
        &["--iterations"],
        false,
    );
    push_node(&mut nodes, "dev capture", Vec::new(), &["--layout"], false);
    push_node(
        &mut nodes,
        "dev dump",
        names(DEV_DUMP_SUBVERBS),
        &["--rows", "--cols"],
        false,
    );
    push_node(
        &mut nodes,
        "dev overlay",
        names(DEV_OVERLAY_SUBVERBS),
        &[],
        false,
    );
    push_node(
        &mut nodes,
        "dev overlay show",
        names(OverlayName::NAMES),
        &[],
        false,
    );

    let plugin_verbs: Vec<&'static str> = PluginVerb::VERBS.iter().map(|(name, _)| *name).collect();
    push_node(
        &mut nodes,
        "plugin",
        plugin_verbs,
        &[
            "--format",
            "--no-color",
            "--yes",
            "--force",
            "--cap",
            "--profile",
        ],
        false,
    );
    let config_actions: Vec<&'static str> = ConfigAction::ACTIONS
        .iter()
        .map(|(name, _)| *name)
        .collect();
    push_node(&mut nodes, "plugin config", config_actions, &[], false);

    let component_verbs: Vec<&'static str> =
        ComponentVerb::VERBS.iter().map(|(name, _)| *name).collect();
    push_node(
        &mut nodes,
        "extension",
        component_verbs,
        &[
            "--format",
            "--no-color",
            "--version",
            "--digest",
            "--distro",
            "--yes",
            "--force",
        ],
        false,
    );

    push_node(&mut nodes, "version", Vec::new(), &["--format"], false);

    let shells = names(CompletionShell::all());
    push_node(&mut nodes, "completion", shells.clone(), &[], false);
    push_node(&mut nodes, "shell-init", shells, &[], false);

    push_node(&mut nodes, "cmd", Vec::new(), &["--format"], true);
    push_node(&mut nodes, "x", Vec::new(), &["--format"], true);
    push_node(&mut nodes, "help", top, &[], false);

    // Stable aliases (`cfg`, `ls`, `comp`) resolve exactly like their
    // canonical word, so `bitty cfg <TAB>` completes `path check edit
    // profile` instead of falling off the grammar. Clone the canonical
    // node's candidate list under the alias path.
    let canonical: Vec<(String, Vec<&'static str>, bool)> = nodes
        .iter()
        .map(|node| (node.path.clone(), node.candidates.clone(), node.freeform))
        .collect();
    for (name, aliases) in SUBCOMMANDS {
        let Some((_, candidates, freeform)) = canonical.iter().find(|(path, _, _)| path == name)
        else {
            continue;
        };
        for alias in *aliases {
            nodes.push(Node {
                path: (*alias).to_string(),
                candidates: candidates.clone(),
                freeform: *freeform,
            });
        }
    }

    nodes
}

/// Every flag that consumes a value, for path resolution in generated shells.
pub(crate) fn value_flags() -> Vec<&'static str> {
    let mut flags: Vec<&'static str> = GLOBAL_VALUE_FLAGS.to_vec();
    for flag in NODE_VALUE_FLAGS {
        if !flags.contains(flag) {
            flags.push(*flag);
        }
    }
    flags
}

/// Completion candidates for a resolved path (`""` is the root).
/// Returns `None` when the path is not part of the grammar (or has run past
/// a free-form tail), so shells offer nothing rather than a stale list.
#[cfg(test)]
pub(crate) fn candidates_for(path: &str) -> Option<Vec<&'static str>> {
    nodes()
        .into_iter()
        .find(|node| node.path == path)
        .map(|node| node.candidates)
}

/// Whether the node at `path` accepts arbitrary operands after it.
#[cfg(test)]
pub(crate) fn is_freeform(path: &str) -> bool {
    nodes()
        .iter()
        .any(|node| node.path == path && node.freeform)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::CaptureLayout;

    #[test]
    fn root_offers_every_subcommand_and_alias() {
        let root = candidates_for("").expect("root node");
        for (name, aliases) in SUBCOMMANDS {
            assert!(root.contains(name), "root missing {name}");
            for alias in *aliases {
                assert!(root.contains(alias), "root missing alias {alias}");
            }
        }
        assert!(
            !root.contains(&"component"),
            "hidden component alias must not be completed"
        );
    }

    #[test]
    fn every_nested_table_is_reachable() {
        for (name, _) in ConfigCommand::VERBS {
            assert!(candidates_for("config").unwrap().contains(name));
        }
        for (name, _) in ProfileVerb::VERBS {
            assert!(candidates_for("config profile").unwrap().contains(name));
        }
        for (name, verbs) in CTL_RESOURCES {
            let parent = candidates_for("ctl").expect("ctl node");
            assert!(parent.contains(name), "ctl missing resource {name}");
            let path = child("ctl", name);
            let node = candidates_for(&path).unwrap_or_else(|| panic!("missing {path}"));
            for verb in *verbs {
                assert!(node.contains(verb), "{path} missing verb {verb}");
            }
        }
        for (name, _) in PluginVerb::VERBS {
            assert!(candidates_for("plugin").unwrap().contains(name));
        }
        for (name, _) in ConfigAction::ACTIONS {
            assert!(candidates_for("plugin config").unwrap().contains(name));
        }
        for (name, _) in ComponentVerb::VERBS {
            assert!(candidates_for("extension").unwrap().contains(name));
        }
        for target in InspectTarget::NAMES {
            assert!(candidates_for("inspect").unwrap().contains(target));
        }
        for kind in ListKind::NAMES {
            assert!(candidates_for("list").unwrap().contains(kind));
        }
        for verb in DEV_VERBS {
            assert!(candidates_for("dev").unwrap().contains(verb));
        }
        for sub in DEV_TRACE_SUBVERBS {
            assert!(candidates_for("dev trace").unwrap().contains(sub));
        }
        for sub in DEV_DUMP_SUBVERBS {
            assert!(candidates_for("dev dump").unwrap().contains(sub));
        }
        for sub in DEV_OVERLAY_SUBVERBS {
            assert!(candidates_for("dev overlay").unwrap().contains(sub));
        }
        for overlay in OverlayName::NAMES {
            assert!(
                candidates_for("dev overlay show")
                    .unwrap()
                    .contains(overlay)
            );
        }
        for layout in CaptureLayout::NAMES {
            assert!(candidates_for("dev capture").unwrap().contains(&"--layout"));
            assert!(CaptureLayout::parse(layout).is_some());
        }
        for shell in CompletionShell::all() {
            assert!(candidates_for("completion").unwrap().contains(shell));
            assert!(candidates_for("shell-init").unwrap().contains(shell));
        }
    }

    #[test]
    fn freeform_nodes_are_marked() {
        for path in ["run", "cmd", "x"] {
            assert!(is_freeform(path), "{path} accepts arbitrary operands");
        }
        for path in ["", "config", "config path", "plugin", "ctl workspace"] {
            assert!(!is_freeform(path), "{path} is a closed grammar node");
        }
    }

    #[test]
    fn unknown_paths_have_no_candidates() {
        for path in ["bogus", "config bogus", "ctl workspace bogus", "dev bogus"] {
            assert!(
                candidates_for(path).is_none(),
                "path {path} must be unknown"
            );
        }
    }

    #[test]
    fn global_flags_are_offered_everywhere() {
        for node in nodes() {
            for flag in GLOBAL_FLAGS {
                assert!(
                    node.offers(flag),
                    "node {:?} missing global flag {flag}",
                    node.path
                );
            }
        }
    }

    #[test]
    fn value_flags_are_real_grammar_flags() {
        for flag in value_flags() {
            assert!(
                nodes().iter().any(|node| node.offers(flag)),
                "value flag {flag} is not part of the completion grammar"
            );
        }
    }
}
