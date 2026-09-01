//! Bundled first-party plugin catalog for dogfooding the public Plugin API.
//!
//! This module defines the **exact** accepted bundled-disabled set for `v1`
//! per the Default Distribution RFC (`OQ-002`, accepted 2026-08-29) and the
//! Plugin Roadmap (`bitty-terminal.shell-integration`, `tabs`, `statusline`,
//! `palette`, `project`, `file-manager`, `git-panel`, `browser-panel`). It
//! exists **only** as
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
    CapabilityRequests, Compat, FilesystemRequest, FsAccess, LazyTriggers, PluginId,
    PluginIdentity, PluginManifest, QualifiedName,
};

/// Canonical version for the five `v1` bundled plugins (SemVer 2).
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
        capabilities: caps,
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

/// `bitty-terminal.tabs` — tab commands, tabline presentation, ordering,
/// key bindings, and closing policy.
///
/// Capability: `ui.rich` (tabline presentation via rich primitives).
/// Claims: `tabline` exclusive (register vs claim semantics, duplicate
/// claim is diagnosed not last-wins).
/// Commands reserve tab actions at graph construction.
#[must_use]
pub fn tabs_manifest() -> PluginManifest {
    let mut caps = CapabilityRequests::default();
    caps.ids
        .insert(CapabilityId::parse("ui.rich").expect("known capability"));
    PluginManifest {
        identity: bundled_identity(
            "bitty-terminal.tabs",
            "Tabs",
            "Tab commands, tabline presentation, ordering and closing policy",
        ),
        compat: bundled_compat(),
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        capabilities: caps,
        lazy: LazyTriggers {
            commands: vec![
                QualifiedName::new("bitty-terminal.tabs:new").expect("qualified"),
                QualifiedName::new("bitty-terminal.tabs:close").expect("qualified"),
                QualifiedName::new("bitty-terminal.tabs:next").expect("qualified"),
            ],
            events: vec![
                "terminal.title-changed".to_string(),
                "focus.changed".to_string(),
            ],
            claims: vec!["tabline".to_string()],
        },
        raw_bytes_len: 512,
    }
}

/// `bitty-terminal.statusline` — presentation of cwd, mode, Git and task
/// state via status-component composition.
///
/// Capability: `terminal.semantic-read` (cwd/mode snapshot) plus
/// status-component composition (no terminal write).
/// Events: `terminal.cwd-changed`, `terminal.title-changed`.
#[must_use]
pub fn statusline_manifest() -> PluginManifest {
    let mut caps = CapabilityRequests::default();
    caps.ids
        .insert(CapabilityId::parse("terminal.semantic-read").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("ui.rich").expect("known capability"));
    PluginManifest {
        identity: bundled_identity(
            "bitty-terminal.statusline",
            "Statusline",
            "Cwd, mode, Git and task presentation via status-component composition",
        ),
        compat: bundled_compat(),
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        capabilities: caps,
        lazy: LazyTriggers {
            commands: Vec::new(),
            events: vec![
                "terminal.cwd-changed".to_string(),
                "terminal.title-changed".to_string(),
            ],
            claims: Vec::new(),
        },
        raw_bytes_len: 512,
    }
}

/// `bitty-terminal.palette` — command palette and picker UI via overlay
/// slot using declarative list/text primitives only (no shader/native
/// window).
///
/// Capability: `ui.overlay`
#[must_use]
pub fn palette_manifest() -> PluginManifest {
    let mut caps = CapabilityRequests::default();
    caps.ids
        .insert(CapabilityId::parse("ui.overlay").expect("known capability"));
    PluginManifest {
        identity: bundled_identity(
            "bitty-terminal.palette",
            "Palette",
            "Command palette and picker UI via overlay slot, declarative primitives only",
        ),
        compat: bundled_compat(),
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        capabilities: caps,
        lazy: LazyTriggers {
            commands: vec![QualifiedName::new("bitty-terminal.palette:toggle").expect("qualified")],
            events: vec!["focus.changed".to_string()],
            claims: Vec::new(),
        },
        raw_bytes_len: 512,
    }
}

/// `bitty-terminal.project` — project discovery and session presentation.
///
/// Capability: `fs.read:~/projects/**` constrained via filesystem request
/// (path-glob, real-path resolved, symlinks/devices rejected per host
/// policy). Also `terminal.semantic-read` for cwd context.
/// No `fs.write`, no `process.spawn`, no `network.*`.
#[must_use]
pub fn project_manifest() -> PluginManifest {
    let mut caps = CapabilityRequests::default();
    caps.ids
        .insert(CapabilityId::parse("terminal.semantic-read").expect("known capability"));
    caps.filesystem.push(FilesystemRequest {
        access: FsAccess::Read,
        paths: vec!["~/projects/**".to_string()],
    });
    PluginManifest {
        identity: bundled_identity(
            "bitty-terminal.project",
            "Project",
            "Project discovery and session presentation with constrained fs.read",
        ),
        compat: bundled_compat(),
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        capabilities: caps,
        lazy: LazyTriggers {
            commands: vec![
                QualifiedName::new("bitty-terminal.project:open").expect("qualified"),
                QualifiedName::new("bitty-terminal.project:switch").expect("qualified"),
            ],
            events: vec!["terminal.cwd-changed".to_string()],
            claims: Vec::new(),
        },
        raw_bytes_len: 512,
    }
}

/// `bitty-terminal.file-manager` — tiled `Panel(PanelId)` file manager.
///
/// Capability: `panel.provider` + `panel.create` for Panel Runtime plus
/// `fs.read:~/projects/**` (read-only listing via path-glob) and optional
/// `fs.write:~/projects/**` for user-confirmed mutations (rename/move/copy).
/// Also `terminal.semantic-read` for cwd context and title observation.
/// No `process.spawn`, no `network.*` — bounded `8 KiB`/`32`/`64`/
/// `1024`/`8192` `DropOldest`, PR-1..PR-12, single-process `winit`.
#[must_use]
pub fn file_manager_manifest() -> PluginManifest {
    let mut caps = CapabilityRequests::default();
    caps.ids
        .insert(CapabilityId::parse("panel.provider").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("panel.create").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("terminal.semantic-read").expect("known capability"));
    caps.filesystem.push(FilesystemRequest {
        access: FsAccess::Read,
        paths: vec!["~/projects/**".to_string()],
    });
    caps.filesystem.push(FilesystemRequest {
        access: FsAccess::Write,
        paths: vec!["~/projects/**".to_string()],
    });
    PluginManifest {
        identity: bundled_identity(
            "bitty-terminal.file-manager",
            "File Manager",
            "Tiled Panel file manager with fs.read + optional fs.write, bounded 8KiB/32/64 PR-1..12",
        ),
        compat: bundled_compat(),
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        capabilities: caps,
        lazy: LazyTriggers {
            commands: vec![
                QualifiedName::new("bitty-terminal.file-manager:open").expect("qualified"),
                QualifiedName::new("bitty-terminal.file-manager:preview").expect("qualified"),
                QualifiedName::new("bitty-terminal.file-manager:rename").expect("qualified"),
            ],
            events: vec![
                "terminal.cwd-changed".to_string(),
                "terminal.title-changed".to_string(),
                "focus.changed".to_string(),
            ],
            claims: Vec::new(),
        },
        raw_bytes_len: 512,
    }
}

/// `bitty-terminal.git-panel` — tiled `Panel(PanelId)` git panel.
///
/// Capability: `panel.provider` + `panel.create` for Panel Runtime plus
/// `process.spawn:git` allowlisted `[tools.git]` bounded `8 KiB`/`32` and
/// `terminal.semantic-read` for cwd/link context plus optional
/// `fs.read:~/projects/**` for working-tree read. System CLI reuse via
/// `process.spawn:git(...)` with manifest-declared `[tools.git]` allowlist
/// (per Layer 2 of `plugin-reuse-and-providers.md`), allowlisted `git` CLI
/// outputs piped to panel UI, not raw PTY injection. Bounded `8 KiB`/`32`/
/// `64`/`1024`/`8192` `DropOldest`, PR-1..PR-12, single-process `winit`,
/// `is_untrusted_surface = true`, RC-1/RC-2 attribution per generation.
#[must_use]
pub fn git_panel_manifest() -> PluginManifest {
    let mut caps = CapabilityRequests::default();
    caps.ids
        .insert(CapabilityId::parse("panel.provider").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("panel.create").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("terminal.semantic-read").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("process.spawn:git").expect("known capability"));
    caps.filesystem.push(FilesystemRequest {
        access: FsAccess::Read,
        paths: vec!["~/projects/**".to_string()],
    });
    PluginManifest {
        identity: bundled_identity(
            "bitty-terminal.git-panel",
            "Git Panel",
            "Tiled Panel git branch/status/diff/log via process.spawn:git allowlisted [tools.git] bounded 8KiB/32/64 PR-1..12",
        ),
        compat: bundled_compat(),
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        capabilities: caps,
        lazy: LazyTriggers {
            commands: vec![
                QualifiedName::new("bitty-terminal.git-panel:open").expect("qualified"),
                QualifiedName::new("bitty-terminal.git-panel:status").expect("qualified"),
                QualifiedName::new("bitty-terminal.git-panel:diff").expect("qualified"),
                QualifiedName::new("bitty-terminal.git-panel:log").expect("qualified"),
                QualifiedName::new("bitty-terminal.git-panel:branch").expect("qualified"),
            ],
            events: vec![
                "terminal.cwd-changed".to_string(),
                "terminal.title-changed".to_string(),
                "focus.changed".to_string(),
            ],
            claims: Vec::new(),
        },
        raw_bytes_len: 512,
    }
}

/// `bitty-terminal.browser-panel` — `View Browser(BrowserSurfaceId)` host surface + `Panel(PanelId)` controls.
///
/// Capability: `panel.provider` + `panel.create` for Panel controls plus
/// `browser.embed` high-risk + `browser.navigation` + `browser.file-url`
/// for `file://` + `browser.storage` for cookie/cache persistence (each a
/// distinct gate). Host-owned `BrowserSurfaceId` per `05e8803` placement
/// Option A, `LogicalRect` placement per `View`, host-mediated
/// `browser.navigate` with allowlist (`https` default, `file` needs
/// `browser.file-url` gate per R-005 `FileUrlActivation`), focus reuse
/// (`focused View` owns keyboard/IME/wheel). Bounded `8 KiB`/`32`/`64`/
/// `1024`/`8192` `DropOldest`, PR-1..PR-12, BA-1 `4`/BA-2 `1`/BA-3 `32`
/// single-process `winit`, embedder under RC-3 `512 MiB` aggregate,
/// `is_untrusted_surface = true` for web content.
#[must_use]
pub fn browser_panel_manifest() -> PluginManifest {
    let mut caps = CapabilityRequests::default();
    caps.ids
        .insert(CapabilityId::parse("panel.provider").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("panel.create").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("browser.embed").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("browser.navigation").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("browser.file-url").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("browser.storage").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("terminal.semantic-read").expect("known capability"));
    PluginManifest {
        identity: bundled_identity(
            "bitty-terminal.browser-panel",
            "Browser Panel",
            "View Browser(BrowserSurfaceId) + Panel(PanelId) tiled, browser.embed/navigation/file-url/storage allowlisted 8KiB/32 BA-1..3",
        ),
        compat: bundled_compat(),
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        capabilities: caps,
        lazy: LazyTriggers {
            commands: vec![
                QualifiedName::new("bitty-terminal.browser-panel:open").expect("qualified"),
                QualifiedName::new("bitty-terminal.browser-panel:navigate").expect("qualified"),
                QualifiedName::new("bitty-terminal.browser-panel:back").expect("qualified"),
                QualifiedName::new("bitty-terminal.browser-panel:forward").expect("qualified"),
                QualifiedName::new("bitty-terminal.browser-panel:reload").expect("qualified"),
            ],
            events: vec![
                "terminal.cwd-changed".to_string(),
                "terminal.title-changed".to_string(),
                "focus.changed".to_string(),
            ],
            claims: Vec::new(),
        },
        raw_bytes_len: 512,
    }
}

/// `bitty-terminal.ai-panel` — tiled `Panel(PanelId)` agent surface plus
/// `AgentId`/`AgentWorkspace` `32 KiB` budget, `mcp.invoke`.
///
/// Capability: `panel.provider` + `panel.create` for Panel plus
/// `agent.context.terminal` per `Terminal` with generation +
/// `agent.context.workspace` per `Workspace` +
/// `agent.memory:persist` opt-in only (`0600`, `<=7 days`, no exfiltration) +
/// `mcp.invoke:TOOL` per-tool capability (e.g. `mcp.invoke:read_file`) +
/// `ai.provider` + `ai.stream` (`ai.model`).
///
/// `AgentId` `owner.name` bounded `128` (`a.b` grammar),
/// `AgentWorkspace` ephemeral `64` files / `2 MiB` aggregate /
/// `256 KiB` per file, `ContextProvider` set with `32 KiB` Context Budget per
/// turn, `AgentMemory` conversational `32` turns / `64 KiB` aggregate;
/// Tool Bus via MCP adapter bounded framing `256 KiB` frame,
/// `512 KiB` in-flight, depth `32`, `RC-9`/`RC-10`.
#[must_use]
pub fn ai_panel_manifest() -> PluginManifest {
    let mut caps = CapabilityRequests::default();
    caps.ids
        .insert(CapabilityId::parse("panel.provider").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("panel.create").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("agent.context.terminal").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("agent.context.workspace").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("agent.memory:persist").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("mcp.invoke:read_file").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("mcp.invoke:fetch").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("ai.provider").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("ai.stream").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("ai.model").expect("known capability"));
    PluginManifest {
        identity: bundled_identity(
            "bitty-terminal.ai-panel",
            "AI Panel",
            "Tiled Panel ai-panel with AgentId/AgentWorkspace 32KiB budget mcp.invoke bounded Panel(PanelId) BA-7..10",
        ),
        compat: bundled_compat(),
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        capabilities: caps,
        lazy: LazyTriggers {
            commands: vec![
                QualifiedName::new("bitty-terminal.ai-panel:open").expect("qualified"),
                QualifiedName::new("bitty-terminal.ai-panel:send").expect("qualified"),
                QualifiedName::new("bitty-terminal.ai-panel:clear").expect("qualified"),
                QualifiedName::new("bitty-terminal.ai-panel:new-session").expect("qualified"),
                QualifiedName::new("bitty-terminal.ai-panel:stop").expect("qualified"),
            ],
            events: vec![
                "terminal.cwd-changed".to_string(),
                "terminal.title-changed".to_string(),
                "focus.changed".to_string(),
            ],
            claims: Vec::new(),
        },
        raw_bytes_len: 512,
    }
}

// ── catalog helpers ───────────────────────────────────────────────────────

/// All nine bundled-disabled manifests for `v1` (fresh install: staged but
/// not enabled). File-manager is P1 tiled Panel with `fs.read`+optional
/// `fs.write`, git-panel is P1 tiled Panel with `process.spawn:git`
/// allowlisted `[tools.git]`, browser-panel is P2 `View Browser` + `Panel`
/// tiled with `browser.embed`/`navigation`/`file-url`/`storage` allowlisted
/// `https` default, all bounded `8 KiB`/`32`/`64`/PR-1..PR-12/BA-1..3,
/// single-process `winit`.
#[must_use]
pub fn all_bundled_manifests() -> Vec<PluginManifest> {
    vec![
        shell_integration_manifest(),
        tabs_manifest(),
        statusline_manifest(),
        palette_manifest(),
        project_manifest(),
        file_manager_manifest(),
        git_panel_manifest(),
        browser_panel_manifest(),
        ai_panel_manifest(),
    ]
}

/// Plugin ids of the nine bundled-disabled plugins, in catalog order.
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

/// Whether `id` is one of the nine bundled ids.
#[must_use]
pub fn is_bundled(id: &PluginId) -> bool {
    matches!(
        id.as_str(),
        "bitty-terminal.shell-integration"
            | "bitty-terminal.tabs"
            | "bitty-terminal.statusline"
            | "bitty-terminal.palette"
            | "bitty-terminal.project"
            | "bitty-terminal.file-manager"
            | "bitty-terminal.git-panel"
            | "bitty-terminal.browser-panel"
            | "bitty-terminal.ai-panel"
    )
}

/// Lookup a bundled manifest by its fully qualified id string, if present.
#[must_use]
pub fn bundled_manifest_for(id: &str) -> Option<PluginManifest> {
    match id {
        "bitty-terminal.shell-integration" => Some(shell_integration_manifest()),
        "bitty-terminal.tabs" => Some(tabs_manifest()),
        "bitty-terminal.statusline" => Some(statusline_manifest()),
        "bitty-terminal.palette" => Some(palette_manifest()),
        "bitty-terminal.project" => Some(project_manifest()),
        "bitty-terminal.file-manager" => Some(file_manager_manifest()),
        "bitty-terminal.git-panel" => Some(git_panel_manifest()),
        "bitty-terminal.browser-panel" => Some(browser_panel_manifest()),
        "bitty-terminal.ai-panel" => Some(ai_panel_manifest()),
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
        assert_eq!(all.len(), 9);
        for m in &all {
            assert_manifest_valid(m);
        }
        let ids = bundled_ids_sorted();
        assert_eq!(
            ids,
            vec![
                "bitty-terminal.ai-panel",
                "bitty-terminal.browser-panel",
                "bitty-terminal.file-manager",
                "bitty-terminal.git-panel",
                "bitty-terminal.palette",
                "bitty-terminal.project",
                "bitty-terminal.shell-integration",
                "bitty-terminal.statusline",
                "bitty-terminal.tabs",
            ]
        );
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
    fn tabs_manifest_has_tabline_claim_and_commands() {
        let m = tabs_manifest();
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("ui.rich").unwrap())
        );
        assert!(m.lazy.claims.contains(&"tabline".to_string()));
        assert!(m.lazy.commands.len() >= 2);
    }

    #[test]
    fn statusline_manifest_capabilities() {
        let m = statusline_manifest();
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("terminal.semantic-read").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("ui.rich").unwrap())
        );
    }

    #[test]
    fn palette_manifest_capabilities() {
        let m = palette_manifest();
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("ui.overlay").unwrap())
        );
        assert_eq!(m.lazy.commands.len(), 1);
    }

    #[test]
    fn project_manifest_filesystem_capability() {
        let m = project_manifest();
        assert_eq!(m.capabilities.filesystem.len(), 1);
        assert_eq!(m.capabilities.filesystem[0].paths, vec!["~/projects/**"]);
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("terminal.semantic-read").unwrap())
        );
        // filesystem expansion must parse as valid capability
        let expanded = CapabilityId::parse("fs.read:~/projects/**").unwrap();
        assert_eq!(expanded.family(), crate::capability::CapabilityFamily::Fs);
        // manifest hash must be deterministic
        assert_eq!(m.manifest_hash(), m.clone().manifest_hash());
    }

    #[test]
    fn file_manager_manifest_filesystem_and_panel_capabilities() {
        let m = file_manager_manifest();
        assert_eq!(m.capabilities.filesystem.len(), 2);
        let read = m
            .capabilities
            .filesystem
            .iter()
            .find(|r| r.access == FsAccess::Read)
            .unwrap();
        assert_eq!(read.paths, vec!["~/projects/**"]);
        let write = m
            .capabilities
            .filesystem
            .iter()
            .find(|r| r.access == FsAccess::Write)
            .unwrap();
        assert_eq!(write.paths, vec!["~/projects/**"]);
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("panel.provider").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("panel.create").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("terminal.semantic-read").unwrap())
        );
        assert_eq!(m.lazy.commands.len(), 3);
        assert!(
            m.lazy
                .commands
                .iter()
                .any(|c| c.as_str() == "bitty-terminal.file-manager:open")
        );
        assert!(m.lazy.events.contains(&"terminal.cwd-changed".to_string()));
        let expanded_read = CapabilityId::parse("fs.read:~/projects/**").unwrap();
        assert_eq!(
            expanded_read.family(),
            crate::capability::CapabilityFamily::Fs
        );
        let expanded_write = CapabilityId::parse("fs.write:~/projects/**").unwrap();
        assert_eq!(
            expanded_write.family(),
            crate::capability::CapabilityFamily::Fs
        );
        assert_eq!(m.manifest_hash(), m.clone().manifest_hash());
        // tiled Panel + fs isolation, no process/network
        assert!(
            !m.capabilities.ids.contains(
                &CapabilityId::parse("network.connect:example.com:443")
                    .unwrap_or_else(|_| CapabilityId::parse("fs.read:~/projects/**").unwrap())
            )
        );
    }

    #[test]
    fn git_panel_manifest_process_spawn_and_panel_capabilities() {
        let m = git_panel_manifest();
        assert_eq!(m.capabilities.filesystem.len(), 1);
        let read = m
            .capabilities
            .filesystem
            .iter()
            .find(|r| r.access == FsAccess::Read)
            .unwrap();
        assert_eq!(read.paths, vec!["~/projects/**"]);
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("panel.provider").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("panel.create").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("terminal.semantic-read").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("process.spawn:git").unwrap())
        );
        assert_eq!(m.lazy.commands.len(), 5);
        assert!(
            m.lazy
                .commands
                .iter()
                .any(|c| c.as_str() == "bitty-terminal.git-panel:open")
        );
        assert!(
            m.lazy
                .commands
                .iter()
                .any(|c| c.as_str() == "bitty-terminal.git-panel:status")
        );
        assert!(m.lazy.events.contains(&"terminal.cwd-changed".to_string()));
        assert!(m.lazy.events.contains(&"focus.changed".to_string()));
        let expanded_read = CapabilityId::parse("fs.read:~/projects/**").unwrap();
        assert_eq!(
            expanded_read.family(),
            crate::capability::CapabilityFamily::Fs
        );
        let expanded_proc = CapabilityId::parse("process.spawn:git").unwrap();
        assert_eq!(
            expanded_proc.family(),
            crate::capability::CapabilityFamily::Process
        );
        assert_eq!(m.manifest_hash(), m.clone().manifest_hash());
        // allowlisted git, not arbitrary process
        assert!(
            !m.capabilities
                .ids
                .contains(&CapabilityId::parse("process.spawn:rg").unwrap())
        );
        assert!(
            !m.capabilities.ids.contains(
                &CapabilityId::parse("network.connect:example.com:443")
                    .unwrap_or_else(|_| CapabilityId::parse("fs.read:~/projects/**").unwrap())
            )
        );
    }

    #[test]
    fn browser_panel_manifest_browser_and_panel_capabilities() {
        let m = browser_panel_manifest();
        assert_eq!(m.capabilities.filesystem.len(), 0);
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("panel.provider").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("panel.create").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("browser.embed").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("browser.navigation").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("browser.file-url").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("browser.storage").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("terminal.semantic-read").unwrap())
        );
        assert_eq!(m.lazy.commands.len(), 5);
        assert!(
            m.lazy
                .commands
                .iter()
                .any(|c| c.as_str() == "bitty-terminal.browser-panel:open")
        );
        assert!(
            m.lazy
                .commands
                .iter()
                .any(|c| c.as_str() == "bitty-terminal.browser-panel:navigate")
        );
        assert!(m.lazy.events.contains(&"terminal.cwd-changed".to_string()));
        assert!(m.lazy.events.contains(&"focus.changed".to_string()));
        assert!(CapabilityId::parse("browser.embed").unwrap().is_high_risk());
        assert!(
            !CapabilityId::parse("browser.navigation")
                .unwrap()
                .is_high_risk()
        );
        assert_eq!(m.manifest_hash(), m.clone().manifest_hash());
        assert!(
            !m.capabilities
                .ids
                .contains(&CapabilityId::parse("process.spawn:git").unwrap())
        );
        assert!(
            !m.capabilities.ids.contains(
                &CapabilityId::parse("network.connect:example.com:443")
                    .unwrap_or_else(|_| CapabilityId::parse("browser.embed").unwrap())
            )
        );
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
