//! `.wheel/` project-definition directory discovery (OQ-068).
//!
//! `OQ-068` is adopted (register accepted 2026-09-23): the project definition
//! lives in a declarative-data-only Git-tracked `.wheel/` tree
//! (`project.toml`, `agents/`, `workflows/`, `prompts/`, `policies/`,
//! `tools/`, `skills/`), discovered as `.wheel/` first with `.agents/` as
//! the compatibility fallback (never a competing source of truth). This
//! module implements that discovery order as pure, bounded, fail-closed path
//! logic plus one narrow filesystem seam ([`discover_on_fs`]); the schema of
//! `project.toml`, the tracked-vs-state split, and trust enforcement stay
//! open, so a definition is located but never read, executed, or trusted
//! here.
//!
//! There is no `unsafe`, no network, and no new dependency (`std` only).

use std::path::{Path, PathBuf};

/// Project-definition directory name (adopted; Git-tracked, data only).
pub const WHEEL_DIR_NAME: &str = ".wheel";

/// Compatibility fallback directory name (read-only compat, never authoritative).
pub const AGENTS_DIR_NAME: &str = ".agents";

/// Project manifest file inside `.wheel/`.
pub const WHEEL_PROJECT_FILE_NAME: &str = "project.toml";

/// Maximum ancestor levels walked upward during discovery.
pub const MAX_WHEEL_SEARCH_DEPTH: usize = 32;

/// Which tree supplied the project definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WheelSource {
    /// Adopted `.wheel/` definition.
    Wheel,
    /// `.agents/` compatibility fallback.
    Agents,
}

impl WheelSource {
    /// Stable label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Wheel => "wheel",
            Self::Agents => "agents",
        }
    }
}

impl std::fmt::Display for WheelSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Located project definition: the definition path and which tree supplied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WheelDiscovery {
    /// Path to `.wheel/project.toml`, or to the `.agents/` directory for
    /// compatibility hits. Paths only: nothing here is read or parsed.
    pub path: PathBuf,
    /// Which tree supplied the definition.
    pub source: WheelSource,
}

/// `.wheel/project.toml` path under `dir` (pure join, no I/O).
#[must_use]
pub fn wheel_project_file(dir: &Path) -> PathBuf {
    dir.join(WHEEL_DIR_NAME).join(WHEEL_PROJECT_FILE_NAME)
}

/// Discover the project definition above `start` (OQ-068 order).
///
/// Walks `start` and its ancestors (at most [`MAX_WHEEL_SEARCH_DEPTH`]
/// levels): the nearest `.wheel/project.toml` wins; otherwise the nearest
/// `.agents/` directory wins as compatibility. Returns `None` when neither
/// exists within range. `exists` is injected so tests cover the order without
/// touching the filesystem; [`discover_on_fs`] supplies the live seam.
pub fn discover(start: &Path, exists: &dyn Fn(&Path) -> bool) -> Option<WheelDiscovery> {
    let mut current = Some(start);
    let mut depth = 0;
    while depth < MAX_WHEEL_SEARCH_DEPTH {
        let dir = current?;
        let project = wheel_project_file(dir);
        if exists(&project) {
            return Some(WheelDiscovery {
                path: project,
                source: WheelSource::Wheel,
            });
        }
        let agents = dir.join(AGENTS_DIR_NAME);
        if exists(&agents) {
            return Some(WheelDiscovery {
                path: agents,
                source: WheelSource::Agents,
            });
        }
        current = dir.parent();
        depth += 1;
    }
    None
}

/// Live discovery seam: [`discover`] against the real filesystem.
///
/// Returns paths only — nothing is read, parsed, or trusted here; schema and
/// trust stay open OQ-068 work.
pub fn discover_on_fs(start: &Path) -> Option<WheelDiscovery> {
    discover(start, &|path| path.exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn existing(paths: &[&str]) -> BTreeSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn wheel_wins_over_agents_at_the_same_level() {
        let present = existing(&["/repo/.wheel/project.toml", "/repo/.agents"]);
        let found = discover(Path::new("/repo"), &|path| present.contains(path))
            .expect("a definition must be found");
        assert_eq!(found.source, WheelSource::Wheel);
        assert_eq!(found.path, PathBuf::from("/repo/.wheel/project.toml"));
    }

    #[test]
    fn agents_is_the_compatibility_fallback() {
        let present = existing(&["/repo/.agents"]);
        let found = discover(Path::new("/repo"), &|path| present.contains(path))
            .expect("compat fallback must be found");
        assert_eq!(found.source, WheelSource::Agents);
        assert_eq!(found.path, PathBuf::from("/repo/.agents"));
    }

    #[test]
    fn nearest_ancestor_wins() {
        let present = existing(&["/repo/.wheel/project.toml"]);
        let found = discover(Path::new("/repo/a/b"), &|path| present.contains(path))
            .expect("ancestor definition must be found");
        assert_eq!(found.source, WheelSource::Wheel);
        assert_eq!(found.path, PathBuf::from("/repo/.wheel/project.toml"));
    }

    #[test]
    fn nothing_present_finds_nothing() {
        let present = existing(&[]);
        assert_eq!(
            discover(Path::new("/repo/a"), &|path| present.contains(path)),
            None
        );
    }

    #[test]
    fn search_depth_is_bounded() {
        // A definition beyond the walk budget stays invisible: fail closed
        // instead of climbing without bound.
        let mut deep = PathBuf::from("/repo");
        for _ in 0..MAX_WHEEL_SEARCH_DEPTH + 4 {
            deep.push("nested");
        }
        let top_project = wheel_project_file(Path::new("/repo"));
        let present = existing(&[top_project.to_str().expect("test path is UTF-8")]);
        assert_eq!(
            discover(&deep, &|path| present.contains(path)),
            None,
            "definitions past the depth budget must not resolve"
        );
    }

    #[test]
    fn live_seam_finds_wheel_then_agents_on_fs() {
        // Filesystem coverage for `discover_on_fs` through a unique scratch
        // tree (derived from the process id). A `Drop` guard removes the
        // tree even when a mid-test assertion panics (CTX-0727, #1315), so
        // a failure cannot leak `bitty-wheel-<pid>` into the temp dir.
        struct ScratchGuard {
            root: PathBuf,
        }
        impl Drop for ScratchGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }
        let root = std::env::temp_dir().join(format!("bitty-wheel-{}", std::process::id()));
        let _guard = ScratchGuard { root: root.clone() };
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).expect("scratch tree creates");
        let project = wheel_project_file(&root);
        std::fs::create_dir_all(project.parent().expect("wheel dir")).expect("wheel dir creates");
        std::fs::write(&project, "schema_version = 1\n").expect("project file writes");
        let found = discover_on_fs(&nested).expect("wheel definition must be found");
        assert_eq!(found.source, WheelSource::Wheel);
        assert_eq!(found.path, project);
        std::fs::remove_file(&project).expect("project file removes");
        std::fs::remove_dir(root.join(WHEEL_DIR_NAME)).expect("wheel dir removes");
        std::fs::create_dir_all(root.join(AGENTS_DIR_NAME)).expect("agents dir creates");
        let found = discover_on_fs(&nested).expect("agents fallback must be found");
        assert_eq!(found.source, WheelSource::Agents);
        std::fs::remove_dir_all(&root).expect("scratch tree removes");
    }

    #[test]
    fn source_labels_stable() {
        assert_eq!(WheelSource::Wheel.as_str(), "wheel");
        assert_eq!(WheelSource::Agents.as_str(), "agents");
        assert_eq!(WheelSource::Wheel.to_string(), "wheel");
    }
}
