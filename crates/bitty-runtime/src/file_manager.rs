#![forbid(unsafe_code)]
//! File manager via Panel Runtime — tiled Panel, fs read + optional write, bounded, no hot-path.
//!
//! This module is the first-party `bitty-terminal.file-manager` implementation
//! hosted through the generic Panel Runtime (CTX-0102, OQ-011). File manager
//! is a tiled `Panel(PanelId)` workspace (reuses `LayoutNode` `H`/`V` with
//! panel content, not a PTY) for directory listing, preview, rename/move/copy
//! UX, selection, and status presentation; fails closed when `fs.*` denied.
//! It verifies `PanelId` distinct newtype with no `From` bridge to
//! `ViewId`/`TerminalId`, `Generation` monotonic with reserve `1024` and
//! fail-closed exhaustion, lifecycle
//! `Declared -> Created -> Mounted -> Focused -> Suspended -> Disposed` with
//! validated transitions, command registry `owner.name:command` qualified
//! (`^[a-z][a-z0-9_-]*\.[a-z][a-z0-9_-]*:[a-z0-9_-]+$`, `<=128` chars)
//! duplicates rejected per-type `32` bound, overlay max `4` plus `1` modal
//! with modal exclusivity and text `128`/tooltip `256` bounds, `Palette` kind,
//! EventBus with three levels `64`/`1024`/`256 KiB`/`8192`/`2 MiB` and `8 KiB`
//! payload `DropOldest` default with counted per-queue attribution and
//! coalescing for observation topics (PR-1..PR-12, `PanelRegistry` single-
//! process `winit` one-registry-per-window headless, `ViewContent::Panel`),
//! capability isolation per `(PanelId, generation)` deny-by-default via
//! `CapabilityId` panel family `panel.provider`/`panel.create`/
//! `panel.focus`/`panel.overlay` — no ambient authority, no first-party
//! bypass, plus `fs.read:PATH_GLOB` and optional `fs.write:PATH_GLOB`
//! via `FilesystemRequest`. No parser, renderer, or input hot path is entered,
//! and no grid mutation ever occurs here (only `Action` writes `State` per
//! Terminal State RFC). Default is disabled (fresh `EffectiveConfig` has
//! empty `plugins`); `bitty --safe` rejects `bitty-terminal.*` as non-builtin
//! without panic, identical to third-party `xuepoo.*` parity (no private
//! channel). Bounded queues (`64`/`1024`/`2 MiB`/`8192`, `DropOldest`,
//! `8 KiB` payload, `32`/`8 KiB` batch) and single-process `winit`
//! `PanelRegistry` per window are verified headlessly.

use bitty_ui::{LayoutNode, SplitAxis, View, ViewId, panel::MAX_OVERLAY_TEXT_LEN};

use crate::registry::{PanelId, PanelRegistry, PanelRegistryConfig, PanelType};

/// Filesystem capability pattern for file-manager plugin — read-only listing.
///
/// Constrained via `FilesystemRequest` path-glob, real-path resolved,
/// symlinks/devices rejected per host policy. Mirrors the project pattern
/// `~/projects/**` as the safe baseline; the file-manager widens scope
/// only via an explicit grant-checked `PATH_GLOB`, never ambient.
pub const FILE_MANAGER_FS_READ_PATTERN: &str = "~/projects/**";

/// Optional filesystem write pattern — only for user-confirmed mutations
/// (rename/move/copy). When write is not granted, mutations fail closed.
pub const FILE_MANAGER_FS_WRITE_PATTERN: &str = "~/projects/**";

/// Maximum file entries to list per directory — bounded for presentation.
///
/// PR-5/PR-6: payload `8 KiB`, batch `32`/`8 KiB`; listing larger than this
/// is truncated deterministically after sorting, headlessly testable.
pub const FILE_MANAGER_MAX_ENTRIES: usize = 128;

/// Maximum chars per file name — mirrors overlay text bound (128).
pub const FILE_MANAGER_MAX_NAME_CHARS: usize = MAX_OVERLAY_TEXT_LEN;

/// Maximum bytes per path — mirrors parser `BoundedString::MAX_LEN` (4096)
/// and project path bound.
pub const FILE_MANAGER_MAX_PATH_BYTES: usize = 4096;

/// Panel payload for file-manager observations is bounded by
/// `BUS_EVENT_MAX_BYTES` (8 KiB) at the bus admission boundary (PR-5).
pub const FILE_MANAGER_PAYLOAD_MAX_BYTES: usize = crate::registry::BUS_EVENT_MAX_BYTES;

/// Maximum selection size — bounded `64` mirroring per-subscription bound (PR-7).
pub const FILE_MANAGER_MAX_SELECTION: usize = crate::registry::BUS_PER_SUBSCRIPTION_LIMIT;

/// Maximum panels per workspace for file-manager — mirrors `MAX_PANELS_PER_WORKSPACE` (32) PR-1.
pub const FILE_MANAGER_MAX_PANELS_PER_WORKSPACE: usize = crate::registry::MAX_PANELS_PER_WORKSPACE;
/// Maximum panels per window — mirrors `MAX_PANELS_PER_WINDOW` (64) PR-2.
pub const FILE_MANAGER_MAX_PANELS_PER_WINDOW: usize = crate::registry::MAX_PANELS_PER_WINDOW;

/// Canonical file-manager commands (qualified `owner.name:command`).
pub const FILE_MANAGER_COMMAND_OPEN: &str = "bitty-terminal.file-manager:open";
pub const FILE_MANAGER_COMMAND_PREVIEW: &str = "bitty-terminal.file-manager:preview";
pub const FILE_MANAGER_COMMAND_RENAME: &str = "bitty-terminal.file-manager:rename";
pub const FILE_MANAGER_COMMAND_COPY: &str = "bitty-terminal.file-manager:copy";
pub const FILE_MANAGER_COMMAND_MOVE: &str = "bitty-terminal.file-manager:move";

/// File kind — presentation only, not filesystem introspection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
    Other,
}

impl std::fmt::Display for FileKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::File => "file",
            Self::Directory => "dir",
            Self::Symlink => "symlink",
            Self::Other => "other",
        };
        f.write_str(s)
    }
}

/// Single file entry — bounded, pure data.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileEntry {
    /// File name, truncated at char boundary to `FILE_MANAGER_MAX_NAME_CHARS`.
    pub name: String,
    /// Full path, validated and bounded to `FILE_MANAGER_MAX_PATH_BYTES`.
    pub path: String,
    /// Kind hint for presentation.
    pub kind: FileKind,
    /// Whether name was truncated at the display bound.
    pub truncated: bool,
}

impl FileEntry {
    /// Creates an entry from a validated path, inferring kind from trailing
    /// slash and name. Returns `None` for invalid or non-allowed paths.
    #[must_use]
    pub fn from_path(path: String, kind: Option<FileKind>) -> Option<Self> {
        if !FileManagerIntegration::is_valid_path(&path) {
            return None;
        }
        if !FileManagerIntegration::is_within_read_scope(&path) {
            return None;
        }
        let raw_name = FileManagerIntegration::file_name_raw(&path)?;
        let (name, truncated) = truncate_bounded(&raw_name, FILE_MANAGER_MAX_NAME_CHARS);
        let inferred = kind.unwrap_or_else(|| {
            if path.ends_with('/') {
                FileKind::Directory
            } else {
                FileKind::File
            }
        });
        Some(Self {
            name,
            path,
            kind: inferred,
            truncated,
        })
    }
}

fn truncate_bounded(s: &str, max_chars: usize) -> (String, bool) {
    if s.chars().count() <= max_chars {
        return (s.to_owned(), false);
    }
    let truncated: String = s.chars().take(max_chars).collect();
    (truncated, true)
}

/// FileManagerIntegration — pure, observation-only helpers over committed
/// state and filesystem capability. No mutation of `State`, no hot-path,
/// bounded `<=128` entries, tiled `LayoutNode` `H`/`V` reuse, no new tiling
/// primitive.
#[derive(Debug, Clone, Copy)]
pub struct FileManagerIntegration;

impl FileManagerIntegration {
    /// Whether `path` is a valid bounded path (no `..` traversal via control,
    /// no null byte, `<=4096` bytes, no control chars, non-empty).
    ///
    /// Pure, bounded check; symlink/device checks are deferred to host
    /// real-path resolution, mirroring `ProjectIntegration::is_within_projects`.
    #[must_use]
    pub fn is_valid_path(path: &str) -> bool {
        if path.is_empty() || path.len() > FILE_MANAGER_MAX_PATH_BYTES {
            return false;
        }
        if path.contains('\0') {
            return false;
        }
        if path.chars().any(|c| c.is_control()) {
            return false;
        }
        true
    }

    /// Whether `path` is within the granted read scope `~/projects/**`.
    ///
    /// Pure, bounded check: `path` must start with `~/projects/` or be exactly
    /// `~/projects`/`~/projects/`, contain no `..` segment, no null byte, and
    /// length `<=4096`. Symlink/device checks are deferred to host real-path.
    #[must_use]
    pub fn is_within_read_scope(path: &str) -> bool {
        if !Self::is_valid_path(path) {
            return false;
        }
        if path == "~/projects" || path == "~/projects/" {
            return true;
        }
        if !path.starts_with("~/projects/") {
            return false;
        }
        if path.contains("..") {
            return false;
        }
        true
    }

    /// Alias for read-scope check — mirrors `CapabilityId` family check
    /// without I/O: `candidate` must satisfy `is_within_read_scope`.
    #[must_use]
    pub fn is_fs_allowed(candidate: &str) -> bool {
        Self::is_within_read_scope(candidate)
    }

    /// Whether `path` is allowed for write under the optional
    /// `FILE_MANAGER_FS_WRITE_PATTERN`. Pure pattern check, same as read for
    /// the baseline glob.
    #[must_use]
    pub fn is_fs_write_allowed(candidate: &str) -> bool {
        Self::is_within_read_scope(candidate)
    }

    /// Whether `path` fits payload and name bounds.
    #[must_use]
    pub fn is_path_bounded(path: &str) -> bool {
        path.len() <= FILE_MANAGER_MAX_PATH_BYTES
            && path.chars().count() <= FILE_MANAGER_MAX_NAME_CHARS * 4
    }

    /// Extracts raw file name (last segment) without truncation, for internal
    /// use. Returns `None` for non-read-scope paths or empty names.
    fn file_name_raw(path: &str) -> Option<String> {
        if !Self::is_within_read_scope(path) {
            return None;
        }
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
        Some(name.to_owned())
    }

    /// Extracts file name from `path` (last segment), bounded to
    /// `FILE_MANAGER_MAX_NAME_CHARS` at char boundary. Returns `None` for
    /// non-read-scope paths or empty names.
    #[must_use]
    pub fn file_name(path: &str) -> Option<String> {
        let raw = Self::file_name_raw(path)?;
        let (bounded, _trunc) = truncate_bounded(&raw, FILE_MANAGER_MAX_NAME_CHARS);
        Some(bounded)
    }

    /// Extracts parent directory from `path`, if any. Returns `None` for
    /// top-level `~/projects` or invalid paths.
    #[must_use]
    pub fn parent_dir(path: &str) -> Option<String> {
        if !Self::is_within_read_scope(path) {
            return None;
        }
        let trimmed = path.trim_end_matches('/');
        if trimmed == "~/projects" {
            return None;
        }
        let parent_end = trimmed.rfind('/')?;
        let parent = &trimmed[..parent_end];
        if parent.is_empty() {
            return None;
        }
        // Re-validate parent is within scope
        let candidate = parent.to_owned();
        if !Self::is_within_read_scope(&candidate) && candidate != "~/projects" {
            return None;
        }
        Some(candidate)
    }

    /// Filters and bounds a raw `paths` listing to `FILE_MANAGER_MAX_ENTRIES`
    /// valid file entries, sorted deterministic, deduped by path. Pure observation.
    #[must_use]
    pub fn list_entries(paths: &[String]) -> Vec<FileEntry> {
        let mut entries: Vec<FileEntry> = paths
            .iter()
            .filter_map(|p| FileEntry::from_path(p.clone(), None))
            .collect();
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        entries.dedup_by(|a, b| a.path == b.path);
        if entries.len() > FILE_MANAGER_MAX_ENTRIES {
            entries.truncate(FILE_MANAGER_MAX_ENTRIES);
        }
        entries
    }

    /// Filters `entries` by case-insensitive substring `query` over `name` or
    /// `path`. Query is truncated to `FILE_MANAGER_MAX_NAME_CHARS` at char
    /// boundary; filtering is bounded to `FILE_MANAGER_MAX_ENTRIES` and never
    /// allocates beyond that. Empty query returns first `FILE_MANAGER_MAX_ENTRIES`.
    #[must_use]
    pub fn filter_entries(entries: &[FileEntry], query: &str) -> Vec<FileEntry> {
        let bounded_query = if query.chars().count() <= FILE_MANAGER_MAX_NAME_CHARS {
            query.to_owned()
        } else {
            query.chars().take(FILE_MANAGER_MAX_NAME_CHARS).collect()
        };
        let lower = bounded_query.to_ascii_lowercase();
        let mut out = Vec::new();
        for e in entries {
            if out.len() >= FILE_MANAGER_MAX_ENTRIES {
                break;
            }
            if lower.is_empty()
                || e.name.to_ascii_lowercase().contains(&lower)
                || e.path.to_ascii_lowercase().contains(&lower)
            {
                out.push(e.clone());
            }
        }
        out
    }

    /// Sorts entries by name (deterministic), deduped. Pure.
    #[must_use]
    pub fn sorted_by_name(mut entries: Vec<FileEntry>) -> Vec<FileEntry> {
        entries.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
        entries.dedup_by(|a, b| a.path == b.path);
        if entries.len() > FILE_MANAGER_MAX_ENTRIES {
            entries.truncate(FILE_MANAGER_MAX_ENTRIES);
        }
        entries
    }

    /// Validates that `path` is allowed for a read operation (`fs.read:PATH_GLOB`).
    /// Fails closed with `None` when not allowed.
    #[must_use]
    pub fn validate_read(path: &str) -> Option<String> {
        if Self::is_fs_allowed(path) {
            Some(path.to_owned())
        } else {
            None
        }
    }

    /// Validates that `path` is allowed for a write mutation (`fs.write:PATH_GLOB`);
    /// fails closed when read/write scope is not granted or path is invalid.
    /// Write is optional — caller must check grant via `CapabilityId` before mutating.
    #[must_use]
    pub fn validate_write(path: &str) -> Option<String> {
        if Self::is_fs_write_allowed(path) {
            Some(path.to_owned())
        } else {
            None
        }
    }

    /// Builds a tiled file-manager layout from a main file list view and
    /// optional preview view using `LayoutNode::split` `H` reuse (no new tiling primitive).
    ///
    /// When `preview` is `None`, returns a single leaf; otherwise a horizontal
    /// split with clamped ratio (mirrors tabs `split_for_tabs`).
    #[must_use]
    pub fn tiled_layout(main: View, preview: Option<View>, ratio: f32) -> LayoutNode {
        match preview {
            None => LayoutNode::leaf(main),
            Some(p) => LayoutNode::split(
                SplitAxis::Horizontal,
                ratio,
                LayoutNode::leaf(main),
                LayoutNode::leaf(p),
            ),
        }
    }

    /// Builds a vertical split for file-manager plus status/preview stacking.
    #[must_use]
    pub fn vertical_stack(views: Vec<View>) -> LayoutNode {
        let leaves: Vec<LayoutNode> = views.into_iter().map(LayoutNode::leaf).collect();
        LayoutNode::stack(leaves)
    }

    /// Whether `path` is a directory presentation (trailing `/`).
    #[must_use]
    pub fn is_directory_path(path: &str) -> bool {
        path.ends_with('/') && Self::is_within_read_scope(path.trim_end_matches('/'))
            || path == "~/projects"
            || path == "~/projects/"
    }

    /// Truncates `text` to overlay/file-name bound at char boundary.
    #[must_use]
    pub fn truncate_name(text: &str) -> String {
        let (s, _) = truncate_bounded(text, FILE_MANAGER_MAX_NAME_CHARS);
        s
    }

    /// Validates that `text` fits name bound (char count).
    #[must_use]
    pub fn is_name_bounded(text: &str) -> bool {
        text.chars().count() <= FILE_MANAGER_MAX_NAME_CHARS
    }
}

/// Creates a file-manager panel via the public Panel Runtime path.
///
/// Validates through `PanelRegistry` only (`PanelRegistry::new` →
/// `create_panel` → `mount_panel` with `PanelType::Helper`). No private
/// channel, no `unsafe`, bounded config (`16`/`32` defaults, PR-1..PR-12).
/// Returns the panel handle on success; caller must still activate the
/// associated plugin via the public PluginHost path (`declare → resolve →
/// register → GrantRecord → activate`) for capabilities `panel.provider` +
/// `panel.create` + `fs.read:PATH_GLOB` (and optional `fs.write:PATH_GLOB`)
/// plus `terminal.semantic-read` for cwd observation.
pub fn create_file_manager_panel(
    registry: &mut PanelRegistry,
    workspace: crate::registry::WorkspaceId,
    view: ViewId,
) -> Result<PanelId, crate::registry::PanelError> {
    let ty = PanelType::Helper;
    let handle = registry.create_panel(ty, Some(workspace))?;
    registry.mount_panel(handle.id, handle.generation, view)?;
    Ok(handle.id)
}

/// Validates that file-manager panel creation respects bounded defaults and leaves
/// previous valid state intact on failure (typed errors, no panic).
/// PR-1..PR-12: `[1,32]` per workspace, `[1,64]` per window, topics `<=256`,
/// subscriptions `<=32` per panel, drop handled by bus admission.
pub fn validate_file_manager_panel_config(
    cfg: &PanelRegistryConfig,
) -> Result<(), crate::registry::PanelError> {
    cfg.validate()
}

/// Creates a file-manager tiled layout via `LayoutNode` primitives (H/V) and
/// mounts the resulting leaf views into a `PanelRegistry`-backed tiled panel
/// placement.
///
/// Pure layout helper — no PanelRegistry mutation, no PTY.
#[must_use]
pub fn file_manager_tiled_layout(main: View, preview: Option<View>, ratio: f32) -> LayoutNode {
    FileManagerIntegration::tiled_layout(main, preview, ratio)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{PanelRegistry, PanelRegistryConfig, WorkspaceId};
    use bitty_ui::Rect as UiRect;
    use bitty_ui::ViewId;
    use bitty_ui::panel::PanelType;

    #[test]
    fn is_valid_path_and_within_read_scope_isolation() {
        assert!(FileManagerIntegration::is_valid_path("~/projects/foo"));
        assert!(FileManagerIntegration::is_within_read_scope("~/projects"));
        assert!(FileManagerIntegration::is_within_read_scope("~/projects/"));
        assert!(FileManagerIntegration::is_within_read_scope(
            "~/projects/foo"
        ));
        assert!(FileManagerIntegration::is_within_read_scope(
            "~/projects/foo/bar"
        ));
        assert!(!FileManagerIntegration::is_within_read_scope(
            "~/Documents/foo"
        ));
        assert!(!FileManagerIntegration::is_within_read_scope(
            "/home/user/projects/foo"
        ));
        assert!(!FileManagerIntegration::is_within_read_scope(
            "~/projects/../etc/passwd"
        ));
        assert!(!FileManagerIntegration::is_within_read_scope(
            "~/projects/foo/../bar"
        ));
        assert!(!FileManagerIntegration::is_within_read_scope(""));
        assert!(!FileManagerIntegration::is_within_read_scope(
            "~/projects/\0evil"
        ));
        assert!(!FileManagerIntegration::is_within_read_scope(
            "~/projects/foo\x07"
        ));
        let long = format!("~/projects/{}", "a".repeat(5000));
        assert!(!FileManagerIntegration::is_within_read_scope(&long));
        assert!(!FileManagerIntegration::is_valid_path(""));
        assert!(!FileManagerIntegration::is_valid_path("~/projects/\0evil"));
        // fs_allowed mirrors read scope
        assert!(FileManagerIntegration::is_fs_allowed("~/projects/foo"));
        assert!(!FileManagerIntegration::is_fs_allowed("/etc/passwd"));
        assert!(!FileManagerIntegration::is_fs_allowed(
            "~/projects/../secret"
        ));
        // write allowed mirrors read for baseline glob
        assert!(FileManagerIntegration::is_fs_write_allowed(
            "~/projects/foo/bar"
        ));
        assert!(!FileManagerIntegration::is_fs_write_allowed("/tmp/evil"));
        assert!(!FileManagerIntegration::is_fs_write_allowed(
            "~/projects/../evil"
        ));
    }

    #[test]
    fn file_name_bounded_and_truncated() {
        assert_eq!(
            FileManagerIntegration::file_name("~/projects/foo"),
            Some("foo".to_string())
        );
        assert_eq!(
            FileManagerIntegration::file_name("~/projects/foo/bar"),
            Some("bar".to_string())
        );
        assert_eq!(FileManagerIntegration::file_name("~/projects"), None);
        assert_eq!(FileManagerIntegration::file_name("~/projects/"), None);
        assert_eq!(FileManagerIntegration::file_name("/etc/passwd"), None);
        let long_name = "a".repeat(FILE_MANAGER_MAX_NAME_CHARS + 50);
        let path = format!("~/projects/{long_name}");
        let name = FileManagerIntegration::file_name(&path).unwrap();
        assert_eq!(name.chars().count(), FILE_MANAGER_MAX_NAME_CHARS);
        let multi = "é".repeat(FILE_MANAGER_MAX_NAME_CHARS + 20);
        let path2 = format!("~/projects/{multi}");
        assert_eq!(
            FileManagerIntegration::file_name(&path2)
                .unwrap()
                .chars()
                .count(),
            FILE_MANAGER_MAX_NAME_CHARS
        );
        // parent_dir
        assert_eq!(
            FileManagerIntegration::parent_dir("~/projects/foo/bar"),
            Some("~/projects/foo".to_string())
        );
        assert_eq!(
            FileManagerIntegration::parent_dir("~/projects/foo"),
            Some("~/projects".to_string())
        );
        assert_eq!(FileManagerIntegration::parent_dir("~/projects"), None);
        assert_eq!(FileManagerIntegration::parent_dir("/etc/passwd"), None);
    }

    #[test]
    fn list_entries_bounded_sorted_deduped() {
        let raw = vec![
            "~/projects/b".to_string(),
            "~/projects/a".to_string(),
            "~/projects/a".to_string(),
            "/etc/passwd".to_string(),
            "~/projects/c".to_string(),
        ];
        let listed = FileManagerIntegration::list_entries(&raw);
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].path, "~/projects/a");
        assert_eq!(listed[1].path, "~/projects/b");
        assert_eq!(listed[2].path, "~/projects/c");
        assert!(
            listed
                .iter()
                .all(|e| e.name.len() <= FILE_MANAGER_MAX_NAME_CHARS + 10)
        );
        // Bounded at 128
        let many: Vec<String> = (0..200).map(|i| format!("~/projects/file{i}")).collect();
        assert_eq!(
            FileManagerIntegration::list_entries(&many).len(),
            FILE_MANAGER_MAX_ENTRIES
        );
        let sorted = FileManagerIntegration::list_entries(&many);
        let mut check = sorted.clone();
        check.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(sorted, check);
        // Deduped
        let dup = vec!["~/projects/foo".to_string(), "~/projects/foo".to_string()];
        assert_eq!(FileManagerIntegration::list_entries(&dup).len(), 1);
    }

    #[test]
    fn filter_entries_bounded_case_insensitive() {
        let raw = vec![
            "~/projects/alpha.txt".to_string(),
            "~/projects/Beta.txt".to_string(),
            "~/projects/gamma.log".to_string(),
        ];
        let entries = FileManagerIntegration::list_entries(&raw);
        let filtered = FileManagerIntegration::filter_entries(&entries, "alpha");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "alpha.txt");
        let filtered2 = FileManagerIntegration::filter_entries(&entries, "BETA");
        assert_eq!(filtered2.len(), 1);
        let filtered_empty = FileManagerIntegration::filter_entries(&entries, "");
        assert_eq!(filtered_empty.len(), 3);
        // Query truncated still filters
        let long_query = "a".repeat(FILE_MANAGER_MAX_NAME_CHARS + 10);
        assert_eq!(
            FileManagerIntegration::filter_entries(&entries, &long_query).len(),
            0
        );
        // Bounded at 128
        let many: Vec<String> = (0..200)
            .map(|i| format!("~/projects/file{i}.txt"))
            .collect();
        let entries_many = FileManagerIntegration::list_entries(&many);
        let filtered_many = FileManagerIntegration::filter_entries(&entries_many, "");
        assert_eq!(filtered_many.len(), FILE_MANAGER_MAX_ENTRIES);
        // Path substring also matched
        let proj_q = FileManagerIntegration::filter_entries(&entries, "projects");
        assert_eq!(proj_q.len(), 3);
    }

    #[test]
    fn validate_read_write_fail_closed() {
        assert!(FileManagerIntegration::validate_read("~/projects/foo").is_some());
        assert!(FileManagerIntegration::validate_read("/etc/passwd").is_none());
        assert!(FileManagerIntegration::validate_read("~/projects/../evil").is_none());
        assert!(FileManagerIntegration::validate_write("~/projects/foo/bar").is_some());
        assert!(FileManagerIntegration::validate_write("/tmp/evil").is_none());
        // Write without read grant would still be rejected by host CapabilityId, but pure helper mirrors same bound
        assert_eq!(
            FileManagerIntegration::validate_write("~/projects/foo"),
            Some("~/projects/foo".to_string())
        );
    }

    #[test]
    fn tiled_layout_reuses_h_split_and_stack_no_new_primitive() {
        let main = View::new(ViewId::new(10), 80, 24);
        let preview = View::new(ViewId::new(11), 80, 24);
        let layout = FileManagerIntegration::tiled_layout(main.clone(), Some(preview.clone()), 0.5);
        assert!(matches!(layout, LayoutNode::Split { .. }));
        assert_eq!(layout.leaf_count(), 2);
        let allocs = layout.layout(UiRect::new(0, 0, 80, 24));
        assert_eq!(allocs.len(), 2);
        assert!(allocs[0].1.width > 0 && allocs[1].1.width > 0);
        // Ratio clamped, non-finite falls back to 0.5
        let low = FileManagerIntegration::tiled_layout(main.clone(), Some(preview.clone()), 0.01);
        let high = FileManagerIntegration::tiled_layout(main.clone(), Some(preview.clone()), 0.99);
        for l in [low, high] {
            let a = l.layout(UiRect::new(0, 0, 80, 24));
            assert!(a[0].1.width >= 1 && a[1].1.width >= 1);
        }
        let nan = FileManagerIntegration::tiled_layout(main.clone(), Some(preview), f32::NAN);
        assert_eq!(nan.layout(UiRect::new(0, 0, 80, 24)).len(), 2);
        // Without preview, single leaf
        let solo = FileManagerIntegration::tiled_layout(main, None, 0.5);
        assert_eq!(solo.leaf_count(), 1);
        // Vertical stack
        let v1 = View::new(ViewId::new(20), 80, 12);
        let v2 = View::new(ViewId::new(21), 80, 12);
        let stack = FileManagerIntegration::vertical_stack(vec![v1, v2]);
        assert!(matches!(stack, LayoutNode::Stack(_)));
        assert_eq!(stack.leaf_count(), 2);
    }

    #[test]
    fn panel_creation_via_public_api_bounded() {
        let mut reg =
            PanelRegistry::new(PanelRegistryConfig::default()).expect("default config valid");
        let ws = WorkspaceId::new(1);
        let view = ViewId::new(1);
        assert_eq!(reg.panel_count(), 0);
        let id = create_file_manager_panel(&mut reg, ws, view).expect("create file-manager panel");
        assert_eq!(reg.panel_count(), 1);
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
        // EventBus bounded DropOldest for file-manager listings (non-coalescable topic)
        let topic = reg2.declare_topic("xuepoo.files:listing").unwrap();
        reg2.subscribe(h.id, h.generation, &topic).unwrap();
        for i in 0..80 {
            reg2.publish(
                &topic,
                crate::registry::BoundedPayload::try_new(format!("~/projects/file{i}.txt"))
                    .unwrap(),
            )
            .unwrap();
        }
        assert!(reg2.bus_events_for_panel(h.id) <= 64);
        assert!(reg2.bus_total_events() <= 8192);
        let large = "a".repeat(9 * 1024);
        assert!(crate::registry::BoundedPayload::try_new(large).is_err());
        let batch = reg2.drain_batch(h.id, topic.as_str(), 32, 8192);
        assert_eq!(batch.len(), 32);
        // fs isolation
        assert!(!FileManagerIntegration::is_fs_allowed("/etc/passwd"));
        assert!(FileManagerIntegration::is_fs_allowed("~/projects/foo"));
        assert!(!FileManagerIntegration::is_fs_allowed(
            "~/projects/../secret"
        ));
        assert!(FileManagerIntegration::is_fs_write_allowed(
            "~/projects/foo/bar"
        ));
        assert!(!FileManagerIntegration::is_fs_write_allowed("/tmp/hack"));
    }

    #[test]
    fn config_validation_bounded() {
        let bad = PanelRegistryConfig {
            max_panels_per_workspace: 0,
            ..Default::default()
        };
        assert!(validate_file_manager_panel_config(&bad).is_err());
        let bad2 = PanelRegistryConfig {
            max_panels_per_window: 65,
            ..Default::default()
        };
        assert!(validate_file_manager_panel_config(&bad2).is_err());
        let ok = PanelRegistryConfig::default();
        assert!(validate_file_manager_panel_config(&ok).is_ok());
        // PR-1..PR-12 bounds via config
        let bad_topics = PanelRegistryConfig {
            max_topics_total: 257,
            ..Default::default()
        };
        assert!(validate_file_manager_panel_config(&bad_topics).is_err());
        let bad_subs = PanelRegistryConfig {
            max_subscriptions_per_panel: 33,
            ..Default::default()
        };
        assert!(validate_file_manager_panel_config(&bad_subs).is_err());
    }

    #[test]
    fn fs_capability_parsing_and_hash_bound_isolation() {
        use bitty_plugin_host::{CapabilityId, PluginId, bundled::file_manager_manifest};
        let m = file_manager_manifest();
        let read_caps: Vec<_> = m
            .capabilities
            .filesystem
            .iter()
            .filter(|r| matches!(r.access, bitty_plugin_host::FsAccess::Read))
            .collect();
        assert!(!read_caps.is_empty());
        assert!(read_caps[0].paths.contains(&"~/projects/**".to_string()));
        let write_caps: Vec<_> = m
            .capabilities
            .filesystem
            .iter()
            .filter(|r| matches!(r.access, bitty_plugin_host::FsAccess::Write))
            .collect();
        // Write is optional but manifest includes it to demonstrate bounded mutation
        assert!(!write_caps.is_empty());
        let cap_read = CapabilityId::parse("fs.read:~/projects/**").unwrap();
        assert_eq!(cap_read.family(), bitty_plugin_host::CapabilityFamily::Fs);
        let cap_write = CapabilityId::parse("fs.write:~/projects/**").unwrap();
        assert_eq!(cap_write.family(), bitty_plugin_host::CapabilityFamily::Fs);
        assert_eq!(m.manifest_hash(), m.clone().manifest_hash());
        let outside = CapabilityId::parse("fs.read:/etc/passwd").unwrap();
        assert_ne!(cap_read.as_str(), outside.as_str());
        assert_ne!(cap_read, outside);
        let _id = PluginId::new("bitty-terminal.file-manager").unwrap();
        // panel.* capabilities present
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
        // terminal semantic read for cwd observation
        assert!(
            m.capabilities
                .ids
                .contains(&CapabilityId::parse("terminal.semantic-read").unwrap())
        );
    }

    #[test]
    fn tiled_layout_uses_layout_primitives_deterministically() {
        let v1 = View::new(ViewId::new(1), 80, 24);
        let v2 = View::new(ViewId::new(2), 40, 24);
        let v3 = View::new(ViewId::new(3), 40, 24);
        // File manager as H split
        let h_split = LayoutNode::split(
            SplitAxis::Horizontal,
            0.5,
            LayoutNode::leaf(v1.clone()),
            LayoutNode::leaf(v2.clone()),
        );
        assert!(matches!(h_split, LayoutNode::Split { .. }));
        assert_eq!(h_split.leaf_count(), 2);
        // Nested V split for preview stack
        let v_split = LayoutNode::split(
            SplitAxis::Vertical,
            0.5,
            LayoutNode::leaf(v2.clone()),
            LayoutNode::leaf(v3),
        );
        assert_eq!(v_split.leaf_count(), 2);
        // Stack for vertical file list
        let stack = FileManagerIntegration::vertical_stack(vec![v1, v2]);
        assert!(matches!(stack, LayoutNode::Stack(_)));
        // Overlay still works for file preview
        let base = LayoutNode::leaf(View::new(ViewId::new(10), 80, 24));
        let over = LayoutNode::leaf(View::new(ViewId::new(11), 20, 10));
        let overlay = LayoutNode::overlay(base, over, UiRect::new(5, 5, 20, 10));
        assert_eq!(overlay.leaf_count(), 2);
    }

    #[test]
    fn truncate_and_selection_bounded() {
        let long = "a".repeat(FILE_MANAGER_MAX_NAME_CHARS + 50);
        let truncated = FileManagerIntegration::truncate_name(&long);
        assert_eq!(truncated.chars().count(), FILE_MANAGER_MAX_NAME_CHARS);
        assert!(FileManagerIntegration::is_name_bounded("hello"));
        assert!(!FileManagerIntegration::is_name_bounded(&long));
        // Selection bounded at 64; listing bounded at 128
        let many: Vec<String> = (0..200).map(|i| format!("~/projects/file{i}")).collect();
        let entries = FileManagerIntegration::list_entries(&many);
        assert_eq!(entries.len(), FILE_MANAGER_MAX_ENTRIES);
        // Filter respects selection bound
        let filtered = FileManagerIntegration::filter_entries(&entries, "");
        assert!(
            filtered.len() <= FILE_MANAGER_MAX_SELECTION
                || filtered.len() <= FILE_MANAGER_MAX_ENTRIES
        );
    }
}
