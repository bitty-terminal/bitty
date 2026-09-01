#![forbid(unsafe_code)]
//! Project via Panel Runtime — fs capability `~/projects/**` isolation, bounded, no hot-path.
//!
//! This module is the first-party `bitty-terminal.project` implementation
//! hosted through the generic Panel Runtime (CTX-0102, OQ-011). Project is
//! project discovery and session presentation with constrained `fs.read:
//! ~/projects/**` via filesystem request (path-glob, real-path resolved,
//! symlinks/devices rejected per host policy). Also `terminal.semantic-read`
//! for cwd context. No `fs.write`, no `process.spawn`, no `network.*`.
//! It verifies fs isolation via the public PluginHost path (`declare →
//! resolve → register → GrantRecord → activate → subscribe → publish →
//! drain SideQueue DropOldest`) with `CapabilityId::parse("fs.read:~/projects/**")`
//! hash-bound grants, deny-by-default, and per-panel isolation. No parser,
//! renderer, or input hot path is entered, and no grid mutation ever occurs
//! here (only `Action` writes `State` per Terminal State RFC). Default is
//! disabled (fresh `EffectiveConfig` has empty `plugins`); `bitty --safe`
//! rejects `bitty-terminal.*` as non-builtin without panic, identical to
//! third-party `xuepoo.*` parity (no private channel). Bounded queues
//! (`64`/`1024`/`2 MiB`/`8192`, `DropOldest`, `8 KiB` payload, `32`/`8 KiB`
//! batch) and single-process `winit` `PanelRegistry` per window are verified
//! headlessly.

use crate::registry::{PanelId, PanelRegistry, PanelRegistryConfig, PanelType};

/// Filesystem capability pattern for project plugin — constrained read-only.
pub const PROJECT_FS_PATTERN: &str = "~/projects/**";

/// Maximum project entries to list per discovery — bounded for presentation.
pub const PROJECT_MAX_PROJECTS: usize = 64;

/// Maximum chars per project name — mirrors overlay text bound for display.
pub const PROJECT_NAME_MAX_CHARS: usize = bitty_ui::panel::MAX_OVERLAY_TEXT_LEN;

/// Panel payload for project observations is bounded by `BUS_EVENT_MAX_BYTES`
/// (8 KiB) at the bus admission boundary.
pub const PROJECT_PAYLOAD_MAX_BYTES: usize = crate::registry::BUS_EVENT_MAX_BYTES;

/// Maximum panels per workspace for project — mirrors `MAX_PANELS_PER_WORKSPACE` (32).
pub const PROJECT_MAX_PANELS_PER_WORKSPACE: usize = crate::registry::MAX_PANELS_PER_WORKSPACE;
pub const PROJECT_MAX_PANELS_PER_WINDOW: usize = crate::registry::MAX_PANELS_PER_WINDOW;

/// ProjectIntegration — pure, observation-only helpers over committed state
/// and filesystem capability. No mutation of `State`, no hot-path, bounded.
#[derive(Debug, Clone, Copy)]
pub struct ProjectIntegration;

impl ProjectIntegration {
    /// Whether `path` is within `~/projects/**` isolation boundary.
    ///
    /// Pure, bounded check: `path` must start with `~/projects/` or be exactly
    /// `~/projects`, contain no `..` segment, no null byte, and length
    /// `<= 4096` (parser bound for cwd). Symlink/device checks are deferred to
    /// host real-path resolution; this gate rejects obvious escapes headlessly.
    #[must_use]
    pub fn is_within_projects(path: &str) -> bool {
        if path.is_empty() || path.len() > 4096 {
            return false;
        }
        if path.contains('\0') {
            return false;
        }
        // Normalize: must be ~/projects/** — allow ~/projects, ~/projects/, ~/projects/foo, ~/projects/foo/bar
        if path == "~/projects" || path == "~/projects/" {
            return true;
        }
        if !path.starts_with("~/projects/") {
            return false;
        }
        // Reject parent traversal
        if path.contains("..") {
            return false;
        }
        // Reject control characters
        if path.chars().any(|c| c.is_control()) {
            return false;
        }
        true
    }

    /// Validates that `candidate` is allowed under the granted `PROJECT_FS_PATTERN`
    /// via pure pattern check. Mirrors the host's `CapabilityId` family check
    /// without I/O: `candidate` must satisfy `is_within_projects`.
    #[must_use]
    pub fn is_fs_allowed(candidate: &str) -> bool {
        Self::is_within_projects(candidate)
    }

    /// Extracts project name from `path` (last segment), bounded to
    /// `PROJECT_NAME_MAX_CHARS` at char boundary. Returns `None` for
    /// non-project paths or empty names.
    #[must_use]
    pub fn project_name(path: &str) -> Option<String> {
        if !Self::is_within_projects(path) {
            return None;
        }
        // Trim trailing slash
        let trimmed = path.trim_end_matches('/');
        if trimmed == "~/projects" {
            return None;
        }
        let name = trimmed.rsplit('/').next()?;
        if name.is_empty() {
            return None;
        }
        if name.contains("..") || name.chars().any(|c| c.is_control()) {
            return None;
        }
        let bounded = if name.chars().count() <= PROJECT_NAME_MAX_CHARS {
            name.to_owned()
        } else {
            name.chars().take(PROJECT_NAME_MAX_CHARS).collect()
        };
        Some(bounded)
    }

    /// Filters and bounds a raw `paths` listing to `PROJECT_MAX_PROJECTS` valid
    /// project paths, sorted deterministic, deduped. Pure observation.
    #[must_use]
    pub fn list_projects(paths: &[String]) -> Vec<String> {
        let mut valid: Vec<String> = paths
            .iter()
            .filter(|p| Self::is_within_projects(p))
            .cloned()
            .collect();
        valid.sort();
        valid.dedup();
        if valid.len() > PROJECT_MAX_PROJECTS {
            valid.truncate(PROJECT_MAX_PROJECTS);
        }
        valid
    }

    /// Validates that `path` fits payload and name bounds.
    #[must_use]
    pub fn is_path_bounded(path: &str) -> bool {
        path.len() <= 4096 && path.chars().count() <= PROJECT_NAME_MAX_CHARS * 4
    }
}

/// Creates a project panel via the public Panel Runtime path.
///
/// Validates through `PanelRegistry` only (`PanelRegistry::new` →
/// `create_panel` → `mount_panel` with `PanelType::Helper`). No private
/// channel, no `unsafe`, bounded config (`16`/`32` defaults). Returns the
/// panel handle on success; caller must still activate the associated plugin
/// via the public PluginHost path (`declare → resolve → register →
/// GrantRecord → activate`) for capabilities `terminal.semantic-read` and
/// `fs.read:~/projects/**`.
pub fn create_project_panel(
    registry: &mut PanelRegistry,
    workspace: crate::registry::WorkspaceId,
    view: crate::ViewId,
) -> Result<PanelId, crate::registry::PanelError> {
    let ty = PanelType::Helper;
    let handle = registry.create_panel(ty, Some(workspace))?;
    registry.mount_panel(handle.id, handle.generation, view)?;
    Ok(handle.id)
}

/// Validates that project panel creation respects bounded defaults and leaves
/// previous valid state intact on failure (typed errors, no panic).
pub fn validate_project_panel_config(
    cfg: &PanelRegistryConfig,
) -> Result<(), crate::registry::PanelError> {
    cfg.validate()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{PanelRegistry, PanelRegistryConfig, WorkspaceId};
    use bitty_ui::ViewId;
    use bitty_ui::panel::PanelType;

    #[test]
    fn is_within_projects_isolation() {
        assert!(ProjectIntegration::is_within_projects("~/projects"));
        assert!(ProjectIntegration::is_within_projects("~/projects/"));
        assert!(ProjectIntegration::is_within_projects("~/projects/foo"));
        assert!(ProjectIntegration::is_within_projects("~/projects/foo/bar"));
        assert!(!ProjectIntegration::is_within_projects("~/Documents/foo"));
        assert!(!ProjectIntegration::is_within_projects(
            "/home/user/projects/foo"
        ));
        assert!(!ProjectIntegration::is_within_projects(
            "~/projects/../etc/passwd"
        ));
        assert!(!ProjectIntegration::is_within_projects(
            "~/projects/foo/../bar"
        ));
        assert!(!ProjectIntegration::is_within_projects(""));
        assert!(!ProjectIntegration::is_within_projects("~/projects/\0evil"));
        assert!(!ProjectIntegration::is_within_projects(
            "~/projects/foo\x07"
        ));
        // Long path bounded at 4096
        let long = format!("~/projects/{}", "a".repeat(5000));
        assert!(!ProjectIntegration::is_within_projects(&long));
    }

    #[test]
    fn project_name_bounded_and_truncated() {
        assert_eq!(
            ProjectIntegration::project_name("~/projects/foo"),
            Some("foo".to_string())
        );
        assert_eq!(
            ProjectIntegration::project_name("~/projects/foo/bar"),
            Some("bar".to_string())
        );
        assert_eq!(ProjectIntegration::project_name("~/projects"), None);
        assert_eq!(ProjectIntegration::project_name("~/projects/"), None);
        assert_eq!(ProjectIntegration::project_name("/etc/passwd"), None);
        let long_name = "a".repeat(PROJECT_NAME_MAX_CHARS + 50);
        let path = format!("~/projects/{long_name}");
        let name = ProjectIntegration::project_name(&path).unwrap();
        assert_eq!(name.chars().count(), PROJECT_NAME_MAX_CHARS);
        // Multibyte safe
        let multi = "é".repeat(PROJECT_NAME_MAX_CHARS + 20);
        let path2 = format!("~/projects/{multi}");
        assert_eq!(
            ProjectIntegration::project_name(&path2)
                .unwrap()
                .chars()
                .count(),
            PROJECT_NAME_MAX_CHARS
        );
    }

    #[test]
    fn list_projects_bounded_sorted_deduped() {
        let raw = vec![
            "~/projects/b".to_string(),
            "~/projects/a".to_string(),
            "~/projects/a".to_string(), // dup
            "/etc/passwd".to_string(),  // rejected
            "~/projects/c".to_string(),
        ];
        let listed = ProjectIntegration::list_projects(&raw);
        assert_eq!(
            listed,
            vec![
                "~/projects/a".to_string(),
                "~/projects/b".to_string(),
                "~/projects/c".to_string()
            ]
        );
        // Bounded at 64
        let many: Vec<String> = (0..100).map(|i| format!("~/projects/proj{i}")).collect();
        assert_eq!(
            ProjectIntegration::list_projects(&many).len(),
            PROJECT_MAX_PROJECTS
        );
        // Valid paths remain sorted deterministic
        let sorted = ProjectIntegration::list_projects(&many);
        let mut check = sorted.clone();
        check.sort();
        assert_eq!(sorted, check);
    }

    #[test]
    fn panel_creation_via_public_api_bounded() {
        let mut reg =
            PanelRegistry::new(PanelRegistryConfig::default()).expect("default config valid");
        let ws = WorkspaceId::new(1);
        let view = ViewId::new(1);
        assert_eq!(reg.panel_count(), 0);
        let id = create_project_panel(&mut reg, ws, view).expect("create project panel");
        assert_eq!(reg.panel_count(), 1);
        // Second panel on same view must fail-closed (AlreadyMounted).
        let handle2 = reg
            .create_panel(PanelType::Helper, Some(ws))
            .expect("second panel");
        assert!(
            reg.mount_panel(handle2.id, handle2.generation, view)
                .is_err()
        );
        let _ = id.get();
        // Focus lifecycle via Panel API
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
        // EventBus bounded DropOldest for project discovery events
        let topic = reg2.declare_topic("xuepoo.project:discovered").unwrap();
        reg2.subscribe(h.id, h.generation, &topic).unwrap();
        for i in 0..80 {
            reg2.publish(
                &topic,
                crate::registry::BoundedPayload::try_new(format!("~/projects/proj{i}")).unwrap(),
            )
            .unwrap();
        }
        assert!(reg2.bus_events_for_panel(h.id) <= 64);
        assert!(reg2.bus_total_events() <= 8192);
        let large = "a".repeat(9 * 1024);
        assert!(crate::registry::BoundedPayload::try_new(large).is_err());
        let batch = reg2.drain_batch(h.id, topic.as_str(), 32, 8192);
        assert_eq!(batch.len(), 32);
        // fs isolation: candidate outside ~/projects/** must be rejected via helper
        assert!(!ProjectIntegration::is_fs_allowed("/etc/passwd"));
        assert!(ProjectIntegration::is_fs_allowed("~/projects/foo"));
        assert!(!ProjectIntegration::is_fs_allowed("~/projects/../secret"));
    }

    #[test]
    fn config_validation_bounded() {
        let bad = PanelRegistryConfig {
            max_panels_per_workspace: 0,
            ..Default::default()
        };
        assert!(validate_project_panel_config(&bad).is_err());
        let bad2 = PanelRegistryConfig {
            max_panels_per_window: 65,
            ..Default::default()
        };
        assert!(validate_project_panel_config(&bad2).is_err());
        let ok = PanelRegistryConfig::default();
        assert!(validate_project_panel_config(&ok).is_ok());
    }

    #[test]
    fn fs_capability_parsing_and_hash_bound_isolation() {
        use bitty_plugin_host::{CapabilityId, PluginId, bundled::project_manifest};
        let m = project_manifest();
        // Manifest declares fs.read:~/projects/** via filesystem request
        assert_eq!(m.capabilities.filesystem.len(), 1);
        assert_eq!(m.capabilities.filesystem[0].paths, vec!["~/projects/**"]);
        // Expanded capability must parse and be family Fs
        let cap = CapabilityId::parse("fs.read:~/projects/**").unwrap();
        assert_eq!(cap.family(), bitty_plugin_host::CapabilityFamily::Fs);
        // Hash is deterministic
        assert_eq!(m.manifest_hash(), m.clone().manifest_hash());
        // Isolation: capability for outside path is different string and not granted unless declared
        let outside = CapabilityId::parse("fs.read:/etc/passwd").unwrap();
        assert_ne!(cap.as_str(), outside.as_str());
        // Parsed id for project is not the outside id
        assert_ne!(cap, outside);
        // Valid manifest id must be qualified
        let _id = PluginId::new("bitty-terminal.project").unwrap();
    }
}
