//! Bundled first-party plugin catalog for dogfooding the public Plugin API.
//!
//! This module defines the **exact** accepted bundled-disabled set for `v1`
//! per the Default Distribution RFC (`OQ-002`, accepted 2026-08-29) and the
//! Plugin Roadmap (`bitty-terminal.shell-integration`, `workspace`, `statusline`,
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
        required_services: Vec::new(),
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
            .map(|c| QualifiedName::new(c).expect("qualified"))
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
        lazy: workspace_lazy_triggers(),
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
        required_services: Vec::new(),
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
        required_services: Vec::new(),
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
        required_services: Vec::new(),
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
        required_services: Vec::new(),
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
        required_services: Vec::new(),
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
        required_services: Vec::new(),
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
        required_services: Vec::new(),
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

/// `bitty-terminal.mail-panel` — tiled `Panel(PanelId)` mail panel via
/// helper-process backed `mcp.invoke:mail.*` + `network.connect`.
///
/// Capability: `panel.provider` + `panel.create` for Panel Runtime plus
/// `mcp.invoke:mail.list` / `mail.read` / `mail.send` / `mail.search`
/// (per-tool bounded `8 KiB` frame) + `network.connect:imap.example.com:993`
/// and `smtp.example.com:465` (per-destination allowlist, same hardened
/// scoping as Browser/Agent) + `fs.read:~/mail/**` local cache only +
/// `terminal.semantic-read` for link/title observation. Strictly
/// helper-process / out-of-process (never `dlopen`), `fs.write:~/mail/**`
/// optional for cache write, `browser.file-url` never implied. Helper
/// process is under RC-3 `512 MiB` aggregate and global `8192`/`2 MiB`
/// shared envelope, `SecretField` tokens `0600` bounded retention identical to
/// `ai-panel` minimization, `is_untrusted_surface = true` for any mail
/// content observed via `mcp` (RC-9/RC-10 framing `8 KiB`, counted drops).
#[must_use]
pub fn mail_panel_manifest() -> PluginManifest {
    let mut caps = CapabilityRequests::default();
    caps.ids
        .insert(CapabilityId::parse("panel.provider").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("panel.create").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("terminal.semantic-read").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("mcp.invoke:mail.list").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("mcp.invoke:mail.read").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("mcp.invoke:mail.send").expect("known capability"));
    caps.ids
        .insert(CapabilityId::parse("mcp.invoke:mail.search").expect("known capability"));
    caps.ids.insert(
        CapabilityId::parse("network.connect:imap.example.com:993").expect("known capability"),
    );
    caps.ids.insert(
        CapabilityId::parse("network.connect:smtp.example.com:465").expect("known capability"),
    );
    caps.filesystem.push(FilesystemRequest {
        access: FsAccess::Read,
        paths: vec!["~/mail/**".to_string()],
    });
    caps.filesystem.push(FilesystemRequest {
        access: FsAccess::Write,
        paths: vec!["~/mail/**".to_string()],
    });
    PluginManifest {
        identity: bundled_identity(
            "bitty-terminal.mail-panel",
            "Mail Panel",
            "Tiled Panel mail via mcp.invoke:mail.* + network.connect imap/smtp + fs.read:~/mail/** bounded 8KiB/32/64 PR-1..12",
        ),
        compat: bundled_compat(),
        dependencies: Vec::new(),
        provided_services: Vec::new(),
        required_services: Vec::new(),
        capabilities: caps,
        lazy: LazyTriggers {
            commands: vec![
                QualifiedName::new("bitty-terminal.mail-panel:open").expect("qualified"),
                QualifiedName::new("bitty-terminal.mail-panel:list").expect("qualified"),
                QualifiedName::new("bitty-terminal.mail-panel:read").expect("qualified"),
                QualifiedName::new("bitty-terminal.mail-panel:compose").expect("qualified"),
                QualifiedName::new("bitty-terminal.mail-panel:send").expect("qualified"),
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

/// All ten bundled-disabled manifests for `v1` (fresh install: staged but
/// not enabled). File-manager is P1 tiled Panel with `fs.read`+optional
/// `fs.write`, git-panel is P1 tiled Panel with `process.spawn:git`
/// allowlisted `[tools.git]`, browser-panel is P2 `View Browser` + `Panel`
/// tiled with `browser.embed`/`navigation`/`file-url`/`storage` allowlisted
/// `https` default, ai-panel is P2 `Panel` + `AgentId` bounded `32 KiB`,
/// mail-panel is P3 `Panel` via `mcp.invoke:mail.*` + `network.connect`
/// `~/mail/**`, all bounded `8 KiB`/`32`/`64`/PR-1..PR-12/BA-1..3,
/// single-process `winit`.
#[must_use]
pub fn all_bundled_manifests() -> Vec<PluginManifest> {
    vec![
        shell_integration_manifest(),
        workspace_manifest(),
        statusline_manifest(),
        palette_manifest(),
        project_manifest(),
        file_manager_manifest(),
        git_panel_manifest(),
        browser_panel_manifest(),
        ai_panel_manifest(),
        mail_panel_manifest(),
    ]
}

/// Plugin ids of the ten bundled-disabled plugins, in catalog order.
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

/// Whether `id` is one of the ten bundled ids (canonical) or the deprecated
/// `bitty-terminal.tabs` alias (removal ≥ v0.2.0).
#[must_use]
pub fn is_bundled(id: &PluginId) -> bool {
    matches!(
        id.as_str(),
        "bitty-terminal.shell-integration"
            | "bitty-terminal.workspace"
            | "bitty-terminal.tabs"
            | "bitty-terminal.statusline"
            | "bitty-terminal.palette"
            | "bitty-terminal.project"
            | "bitty-terminal.file-manager"
            | "bitty-terminal.git-panel"
            | "bitty-terminal.browser-panel"
            | "bitty-terminal.ai-panel"
            | "bitty-terminal.mail-panel"
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
        "bitty-terminal.statusline" => Some(statusline_manifest()),
        "bitty-terminal.palette" => Some(palette_manifest()),
        "bitty-terminal.project" => Some(project_manifest()),
        "bitty-terminal.file-manager" => Some(file_manager_manifest()),
        "bitty-terminal.git-panel" => Some(git_panel_manifest()),
        "bitty-terminal.browser-panel" => Some(browser_panel_manifest()),
        "bitty-terminal.ai-panel" => Some(ai_panel_manifest()),
        "bitty-terminal.mail-panel" => Some(mail_panel_manifest()),
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
        assert_eq!(all.len(), 10);
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
                "bitty-terminal.mail-panel",
                "bitty-terminal.palette",
                "bitty-terminal.project",
                "bitty-terminal.shell-integration",
                "bitty-terminal.statusline",
                "bitty-terminal.workspace",
            ]
        );
        // Deprecated alias still resolves + is_bundled, but is not in the canonical list.
        assert!(!ids.contains(&"bitty-terminal.tabs".to_string()));
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
                m.lazy.commands.iter().any(|c| c.as_str() == cmd),
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
    fn mail_panel_manifest_mcp_network_and_panel_capabilities() {
        let m = mail_panel_manifest();
        assert_eq!(m.capabilities.filesystem.len(), 2);
        let read = m
            .capabilities
            .filesystem
            .iter()
            .find(|r| r.access == FsAccess::Read)
            .unwrap();
        assert_eq!(read.paths, vec!["~/mail/**"]);
        let write = m
            .capabilities
            .filesystem
            .iter()
            .find(|r| r.access == FsAccess::Write)
            .unwrap();
        assert_eq!(write.paths, vec!["~/mail/**"]);
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
                .contains(&CapabilityId::parse("mcp.invoke:mail.list").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("mcp.invoke:mail.read").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("mcp.invoke:mail.send").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("mcp.invoke:mail.search").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("network.connect:imap.example.com:993").unwrap())
        );
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("network.connect:smtp.example.com:465").unwrap())
        );
        assert_eq!(m.lazy.commands.len(), 5);
        assert!(
            m.lazy
                .commands
                .iter()
                .any(|c| c.as_str() == "bitty-terminal.mail-panel:open")
        );
        assert!(
            m.lazy
                .commands
                .iter()
                .any(|c| c.as_str() == "bitty-terminal.mail-panel:list")
        );
        assert!(
            m.lazy
                .commands
                .iter()
                .any(|c| c.as_str() == "bitty-terminal.mail-panel:send")
        );
        assert!(m.lazy.events.contains(&"terminal.cwd-changed".to_string()));
        assert!(m.lazy.events.contains(&"focus.changed".to_string()));
        let expanded_read = CapabilityId::parse("fs.read:~/mail/**").unwrap();
        assert_eq!(
            expanded_read.family(),
            crate::capability::CapabilityFamily::Fs
        );
        let expanded_mcp = CapabilityId::parse("mcp.invoke:mail.list").unwrap();
        assert_eq!(
            expanded_mcp.family(),
            crate::capability::CapabilityFamily::Mcp
        );
        let expanded_net = CapabilityId::parse("network.connect:imap.example.com:993").unwrap();
        assert_eq!(
            expanded_net.family(),
            crate::capability::CapabilityFamily::Network
        );
        assert_eq!(m.manifest_hash(), m.clone().manifest_hash());
        // tiled Panel + mcp/network/fs, no browser.embed for this helper path
        assert!(
            !m.capabilities
                .ids
                .contains(&CapabilityId::parse("browser.embed").unwrap())
        );
        assert!(
            !m.capabilities
                .ids
                .contains(&CapabilityId::parse("process.spawn:git").unwrap())
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
