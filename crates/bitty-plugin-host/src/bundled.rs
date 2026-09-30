//! Bundled first-party plugin catalog for dogfooding the public Plugin API.
//!
//! This module defines the **exact** accepted bundled-disabled set for `v1`
//! per the Default Distribution RFC (`OQ-002`, accepted 2026-08-29) and the
//! Plugin Roadmap: two bundled plugins, `bitty-terminal.shell-integration`
//! and `bitty-terminal.workspace` (plus the deprecated `bitty-terminal.tabs`
//! alias that resolves to workspace). `statusline`
//! migrated to an independent first-party package (OQ-053, `CTX-0398`),
//! `palette` migrated to an independent first-party package (OQ-053,
//! `CTX-0397`), `git-panel` migrated to an independent first-party
//! package (OQ-053, `CTX-0400`), `file-manager` migrated to an
//! independent first-party package (OQ-053, `CTX-0399`), `ai-panel`
//! removed, `mail-panel` removed, `project` removed, and `browser-panel`
//! removed (Unix philosophy: Core mechanism only, `CTX-0886`); none is in
//! this catalog. It exists **only** as
//! review evidence that the public Plugin API is complete enough for
//! first-party use — it does not introduce a private channel.
//!
//! # Parity guarantee (no private channel)
//!
//! Every manifest returned here is a plain [`PluginManifest`] built from the
//! same public types (`PluginId`, `CapabilityId`, `QualifiedName`,
//! `FilesystemRequest`, …) that any third-party `bitty-plugin.toml` would
//! use. No host-private import, no ambient authority, no bypass flag. A
//! third-party plugin that declares the same `capabilities`, `lazy`
//! triggers, and `compat` strings would be validated, granted, and
//! lifecycle-managed identically via [`crate::host::PluginHost`]:
//! `declare -> resolve -> register -> activate` with deny-by-default,
//! hash-bound grants, and generation disposal. Tests in this module and in
//! `tests/bundled_dogfood.rs` assert that parity.
//!
//! # Distribution semantics (bundled != enabled)
//!
//! `v1` bundled is staged, disabled by default. A fresh install with no user
//! configuration starts the core only (`EffectiveConfig::default` has an empty
//! `plugins` set). Enabling is an explicit `plugins.<id>.enabled = true`
//! with capability consent and the permission-diff gate for capability-
//! increasing updates. `bitty --safe` skips these plugins exactly as it
//! skips any third-party `xuepoo.*` id — there is no first-party bypass.
//!
//! # Terminal Truth and bounded cold path
//!
//! These plugins are observation-only consumers of committed terminal state:
//! they never write [`bitty_term_state::State`] (only `Action` writes state
//! per the Terminal State RFC), they never touch the PTY/parser hot path,
//! and every host observation crosses the bounded [`crate::host::SideQueue`]
//! (ADR-0003 rule 4, `DropOldest`, per-subscription `64` / per-plugin
//! `1024` / global `8192`) without ever blocking the producer. Drops are
//! counted for `bitty plugin doctor` via [`crate::host::PluginHost`] counters.

use crate::capability::CapabilityId;
use crate::manifest::{
    CapabilityRequests, Compat, LazyCommand, LazyTriggers, PluginId, PluginIdentity,
    PluginManifest, QualifiedName,
};

/// One schema-less lazy command declaration (bundled manifests are static).
fn lazy_command(id: &str) -> LazyCommand {
    LazyCommand {
        id: QualifiedName::new(id).expect("bundled command id must parse"),
        args_schema: None,
        result_schema: None,
    }
}

/// Canonical version for the two `v1` bundled plugins (SemVer 2).
const BUNDLED_VERSION: &str = "0.1.0";

/// Compat range for the bundled set: `>=0.1,<1.0` with Plugin API `^1.0`.
fn bundled_compat() -> Compat {
    Compat {
        bitty: Some(">=0.1,<1.0".to_string()),
        plugin_api: Some("^1.0".to_string()),
    }
}

fn bundled_identity(id: &str, name: &str, description: &str) -> PluginIdentity {
    PluginIdentity {
        id: PluginId::new(id).expect("bundled plugin id must be valid"),
        name: name.to_string(),
        version: BUNDLED_VERSION.to_string(),
        description: description.to_string(),
        license: Some("MIT".to_string()),
    }
}

// ── individual manifests ──────────────────────────────────────────────────

/// `bitty-terminal.shell-integration` — OSC 7/133 semantic zones, cwd and
/// title propagation, prompt/command-region marks.
///
/// Capability: `terminal.semantic-read` (read-only, bounded snapshot).
/// Events: `terminal.cwd-changed`, `terminal.title-changed` (observation).
/// No filesystem/process/network authority.
#[must_use]
pub fn shell_integration_manifest() -> PluginManifest {
    let mut caps = CapabilityRequests::default();
    caps.ids
        .insert(CapabilityId::parse("terminal.semantic-read").expect("known capability"));
    PluginManifest {
        identity: bundled_identity(
            "bitty-terminal.shell-integration",
            "Shell Integration",
            "OSC 7/133 semantic zones, cwd/title propagation, fail-closed fallback when absent",
        ),
        compat: bundled_compat(),
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        required_services: Vec::new(),
        capabilities: caps,
        tools: Vec::new(),
        network: Vec::new(),
        limits: Default::default(),
        lazy: LazyTriggers {
            commands: Vec::new(),
            events: vec![
                "terminal.cwd-changed".to_string(),
                "terminal.title-changed".to_string(),
                "terminal.bell".to_string(),
            ],
            claims: Vec::new(),
        },
        raw_bytes_len: 512,
    }
}

/// Canonical workspace plugin id (`bitty-terminal.workspace`).
pub const WORKSPACE_PLUGIN_ID: &str = "bitty-terminal.workspace";

/// Deprecated tabs plugin id (`bitty-terminal.tabs`, removal ≥ v0.2.0).
pub const TABS_PLUGIN_ID: &str = "bitty-terminal.tabs";

/// Canonical workspace claim (`workspaceline`).
pub const WORKSPACELINE_CLAIM: &str = "workspaceline";

/// Deprecated tabs claim (`tabline`, removal ≥ v0.2.0).
pub const TABLINE_CLAIM: &str = "tabline";

/// Canonical workspace commands (`bitty-terminal.workspace:*`).
pub const WORKSPACE_COMMANDS: &[&str] = &[
    "bitty-terminal.workspace:new",
    "bitty-terminal.workspace:close",
    "bitty-terminal.workspace:next",
];

/// Deprecated tabs commands (`bitty-terminal.tabs:*`, removal ≥ v0.2.0).
pub const TABS_COMMANDS: &[&str] = &[
    "bitty-terminal.tabs:new",
    "bitty-terminal.tabs:close",
    "bitty-terminal.tabs:next",
];

/// Whether `id` is the deprecated `bitty-terminal.tabs` alias (removal ≥ v0.2.0).
#[must_use]
pub fn is_deprecated_bundled_alias(id: &str) -> bool {
    id.trim() == TABS_PLUGIN_ID
}

/// Deprecation warning for the old `bitty-terminal.tabs` id, if applicable.
///
/// Returns `Some(warning)` for the old id, `None` for the canonical id and
/// unknown ids. Callers (`inspect plugin`, `list`, CLI) display this when the
/// old path resolves so scripts keep working with a visible nudge.
#[must_use]
pub fn deprecated_alias_warning(id: &str) -> Option<String> {
    if is_deprecated_bundled_alias(id) {
        Some(format!(
            "deprecated: plugin id '{TABS_PLUGIN_ID}' is an alias for '{WORKSPACE_PLUGIN_ID}' (removal >= v0.2.0); use the workspace id"
        ))
    } else {
        None
    }
}

/// Canonicalize a UI claim to `workspaceline`.
///
/// Accepts both `workspaceline` (canonical) and `tabline` (deprecated alias).
/// Returns `None` for unknown claims.
#[must_use]
pub fn canonicalize_ui_claim(claim: &str) -> Option<&'static str> {
    match claim.trim() {
        "workspaceline" => Some(WORKSPACELINE_CLAIM),
        "tabline" => Some(WORKSPACELINE_CLAIM),
        _ => None,
    }
}

/// Whether `claim` is the deprecated `tabline` alias.
#[must_use]
pub fn is_deprecated_claim(claim: &str) -> bool {
    claim.trim() == TABLINE_CLAIM
}

/// Canonicalize a workspace command to its `bitty-terminal.workspace:*` form.
///
/// Accepts both new (`bitty-terminal.workspace:new|close|next`) and old
/// (`bitty-terminal.tabs:new|close|next`) forms. Returns `None` for unrelated
/// commands.
#[must_use]
pub fn canonicalize_workspace_command(cmd: &str) -> Option<&'static str> {
    match cmd.trim() {
        "bitty-terminal.workspace:new" => Some("bitty-terminal.workspace:new"),
        "bitty-terminal.workspace:close" => Some("bitty-terminal.workspace:close"),
        "bitty-terminal.workspace:next" => Some("bitty-terminal.workspace:next"),
        "bitty-terminal.tabs:new" => Some("bitty-terminal.workspace:new"),
        "bitty-terminal.tabs:close" => Some("bitty-terminal.workspace:close"),
        "bitty-terminal.tabs:next" => Some("bitty-terminal.workspace:next"),
        _ => None,
    }
}

/// Whether `cmd` is a deprecated `bitty-terminal.tabs:*` command alias.
#[must_use]
pub fn is_deprecated_command(cmd: &str) -> bool {
    matches!(
        cmd.trim(),
        "bitty-terminal.tabs:new" | "bitty-terminal.tabs:close" | "bitty-terminal.tabs:next"
    )
}

fn workspace_lazy_triggers() -> LazyTriggers {
    LazyTriggers {
        commands: WORKSPACE_COMMANDS
            .iter()
            .chain(TABS_COMMANDS.iter())
            .map(|c| lazy_command(c))
            .collect(),
        events: vec![
            "terminal.title-changed".to_string(),
            "focus.changed".to_string(),
        ],
        // Canonical first; deprecated alias second so both activate during the window.
        claims: vec![WORKSPACELINE_CLAIM.to_string(), TABLINE_CLAIM.to_string()],
    }
}

/// `bitty-terminal.workspace` — workspace commands, workspaceline presentation, ordering,
/// key bindings, and closing policy.
///
/// A bitty workspace is a tab group within a window (wezterm inverts this:
/// workspace > window > tab > pane).
///
/// Capability: `ui.rich` (workspaceline presentation via rich primitives).
/// Claims: `workspaceline` exclusive (register vs claim semantics, duplicate
/// claim is diagnosed not last-wins); `tabline` remains as a deprecated alias
/// during the compat window (removal ≥ v0.2.0).
/// Commands reserve workspace actions at graph construction (both new
/// `bitty-terminal.workspace:*` and deprecated `bitty-terminal.tabs:*` so old
/// scripts dispatch identically).
#[must_use]
pub fn workspace_manifest() -> PluginManifest {
    let mut caps = CapabilityRequests::default();
    caps.ids
        .insert(CapabilityId::parse("ui.rich").expect("known capability"));
    PluginManifest {
        identity: bundled_identity(
            WORKSPACE_PLUGIN_ID,
            "Workspace",
            "Workspace commands, workspaceline presentation, ordering and closing policy",
        ),
        compat: bundled_compat(),
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        required_services: Vec::new(),
        capabilities: caps,
        tools: Vec::new(),
        network: Vec::new(),
        limits: Default::default(),
        lazy: workspace_lazy_triggers(),
        raw_bytes_len: 512,
    }
}

/// Deprecated `bitty-terminal.tabs` alias (removal ≥ v0.2.0).
///
/// ALIAS, not flag-day per DEC-0032. Resolves identically to
/// [`workspace_manifest`] except for the legacy id/name/description so stored
/// grants (`GrantRecord` binds id+hash), scripts, and third-party `tabline`
/// claimants keep working during the window. New code must use
/// [`workspace_manifest`]. Old id emits [`deprecated_alias_warning`]; new path
/// does not.
#[deprecated(
    since = "0.1.0",
    note = "use workspace_manifest (tabs alias removal >= v0.2.0)"
)]
#[must_use]
pub fn tabs_manifest() -> PluginManifest {
    let mut caps = CapabilityRequests::default();
    caps.ids
        .insert(CapabilityId::parse("ui.rich").expect("known capability"));
    PluginManifest {
        identity: bundled_identity(
            TABS_PLUGIN_ID,
            "Tabs",
            "Tab commands, tabline presentation, ordering and closing policy (deprecated alias for bitty-terminal.workspace)",
        ),
        compat: bundled_compat(),
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        required_services: Vec::new(),
        capabilities: caps,
        tools: Vec::new(),
        network: Vec::new(),
        limits: Default::default(),
        lazy: workspace_lazy_triggers(),
        raw_bytes_len: 512,
    }
}

// ── catalog helpers ───────────────────────────────────────────────────────

/// Both bundled-disabled manifests for `v1` (`shell-integration`,
/// `workspace`; fresh install: staged but not enabled). ai-panel,
/// mail-panel, project and browser-panel were removed per Unix philosophy
/// (CTX-0886): Core provides mechanism only.
#[must_use]
pub fn all_bundled_manifests() -> Vec<PluginManifest> {
    vec![shell_integration_manifest(), workspace_manifest()]
}

/// Plugin ids of the two bundled-disabled plugins, in catalog order.
#[must_use]
pub fn bundled_ids() -> Vec<PluginId> {
    all_bundled_manifests()
        .into_iter()
        .map(|m| m.identity.id)
        .collect()
}

/// Sorted string ids of the bundled set (deterministic for diagnostics).
#[must_use]
pub fn bundled_ids_sorted() -> Vec<String> {
    let mut ids: Vec<String> = bundled_ids().into_iter().map(|id| id.to_string()).collect();
    ids.sort();
    ids
}

/// Whether `id` is one of the two bundled ids (canonical) or the deprecated
/// `bitty-terminal.tabs` alias (removal ≥ v0.2.0).
#[must_use]
pub fn is_bundled(id: &PluginId) -> bool {
    matches!(
        id.as_str(),
        "bitty-terminal.shell-integration" | "bitty-terminal.workspace" | "bitty-terminal.tabs"
    )
}

/// Lookup a bundled manifest by its fully qualified id string, if present.
///
/// Accepts both the canonical `bitty-terminal.workspace` and the deprecated
/// `bitty-terminal.tabs` alias (removal ≥ v0.2.0). Old path resolves via the
/// tabs shim (same commands/claims, legacy id); pair with
/// [`deprecated_alias_warning`] to surface the deprecation. New path does not
/// warn. Safe-mode shape is unchanged: both ids are `bitty-terminal.*` (not
/// `bitty.` prefix) so `--safe` still rejects both — no builtin promotion.
#[must_use]
#[allow(deprecated)]
pub fn bundled_manifest_for(id: &str) -> Option<PluginManifest> {
    match id.trim() {
        "bitty-terminal.shell-integration" => Some(shell_integration_manifest()),
        "bitty-terminal.workspace" => Some(workspace_manifest()),
        "bitty-terminal.tabs" => Some(tabs_manifest()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::PluginManifest;

    fn assert_manifest_valid(m: &PluginManifest) {
        m.validate().expect("bundled manifest must be valid");
        assert!(m.raw_bytes_len <= crate::manifest::MANIFEST_MAX_BYTES);
        assert!(!m.identity.name.trim().is_empty());
        assert!(!m.capabilities.ids.is_empty() || !m.capabilities.filesystem.is_empty());
    }

    #[test]
    fn bundled_manifests_validate_and_have_expected_ids() {
        let all = all_bundled_manifests();
        assert_eq!(all.len(), 2);
        for m in &all {
            assert_manifest_valid(m);
        }
        let ids = bundled_ids_sorted();
        assert_eq!(
            ids,
            vec![
                "bitty-terminal.shell-integration",
                "bitty-terminal.workspace",
            ]
        );
        // CTX-0886: Unix philosophy, Core mechanism only. Removed plugins are not bundled.
        assert!(!ids.contains(&"bitty-terminal.tabs".to_string()));
        assert!(!ids.contains(&"bitty-terminal.palette".to_string()));
        assert!(!ids.contains(&"bitty-terminal.browser-panel".to_string()));
        assert!(!ids.contains(&"bitty-terminal.project".to_string()));
        assert!(!is_bundled(
            &PluginId::new("bitty-terminal.project").unwrap()
        ));
        assert!(!is_bundled(
            &PluginId::new("bitty-terminal.browser-panel").unwrap()
        ));
        assert!(bundled_manifest_for("bitty-terminal.project").is_none());
        assert!(bundled_manifest_for("bitty-terminal.browser-panel").is_none());
        assert!(!is_bundled(
            &PluginId::new("bitty-terminal.palette").unwrap()
        ));
        assert!(!ids.contains(&"bitty-terminal.statusline".to_string()));
        assert!(!is_bundled(
            &PluginId::new("bitty-terminal.statusline").unwrap()
        ));
        assert!(!ids.contains(&"bitty-terminal.git-panel".to_string()));
        assert!(!is_bundled(
            &PluginId::new("bitty-terminal.git-panel").unwrap()
        ));
        assert!(!ids.contains(&"bitty-terminal.file-manager".to_string()));
        assert!(!is_bundled(
            &PluginId::new("bitty-terminal.file-manager").unwrap()
        ));
        assert!(is_bundled(&PluginId::new("bitty-terminal.tabs").unwrap()));
        assert!(is_bundled(
            &PluginId::new("bitty-terminal.workspace").unwrap()
        ));
    }

    #[test]
    fn shell_integration_manifest_capabilities() {
        let m = shell_integration_manifest();
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("terminal.semantic-read").unwrap())
        );
        assert_eq!(m.lazy.events.len(), 3);
        assert!(m.lazy.commands.is_empty());
    }

    #[test]
    fn workspace_manifest_has_workspaceline_claim_and_commands() {
        let m = workspace_manifest();
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("ui.rich").unwrap())
        );
        assert!(m.lazy.claims.contains(&"workspaceline".to_string()));
        // Deprecated alias still present during the window.
        assert!(m.lazy.claims.contains(&"tabline".to_string()));
        // Both new (3) and old (3) commands dispatch identically.
        assert_eq!(m.lazy.commands.len(), 6);
        for cmd in [
            "bitty-terminal.workspace:new",
            "bitty-terminal.workspace:close",
            "bitty-terminal.workspace:next",
            "bitty-terminal.tabs:new",
            "bitty-terminal.tabs:close",
            "bitty-terminal.tabs:next",
        ] {
            assert!(
                m.lazy.commands.iter().any(|c| c.id.as_str() == cmd),
                "missing {cmd}"
            );
        }
    }

    #[test]
    #[allow(deprecated)]
    fn tabs_manifest_alias_has_same_shape_with_deprecation() {
        let m = tabs_manifest();
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("ui.rich").unwrap())
        );
        assert!(m.lazy.claims.contains(&"tabline".to_string()));
        assert!(m.lazy.claims.contains(&"workspaceline".to_string()));
        assert_eq!(m.lazy.commands.len(), 6);
        assert!(is_deprecated_bundled_alias("bitty-terminal.tabs"));
        assert!(!is_deprecated_bundled_alias("bitty-terminal.workspace"));
        assert!(deprecated_alias_warning("bitty-terminal.tabs").is_some());
        assert!(deprecated_alias_warning("bitty-terminal.workspace").is_none());
    }

    #[test]
    fn bundled_ids_recognized() {
        for id in bundled_ids() {
            assert!(is_bundled(&id));
            assert!(bundled_manifest_for(id.as_str()).is_some());
        }
        let third = PluginId::new("xuepoo.example").unwrap();
        assert!(!is_bundled(&third));
        assert!(bundled_manifest_for("xuepoo.example").is_none());
    }

    #[test]
    fn bundled_manifests_have_no_hot_path_events() {
        // v1 bundled plugins are observation-only (no parser/render/input hot-path).
        // They must not subscribe to synthetic hot-path names.
        for m in all_bundled_manifests() {
            for ev in &m.lazy.events {
                assert!(
                    !ev.contains("byte-received")
                        && !ev.contains("cell-changed")
                        && !ev.contains("damage"),
                    "hot-path event must never appear: {ev}"
                );
            }
        }
    }

    #[test]
    fn bundled_manifests_have_bounded_strings() {
        for m in all_bundled_manifests() {
            assert!(m.identity.name.len() <= 128);
            assert!(m.identity.description.len() <= 1024);
        }
    }
}
