#![forbid(unsafe_code)]
//! Single source of truth for the first-party project filesystem scope.
//!
//! `~/projects` is defined exactly once, as a literal token expanded into
//! [`PROJECT_ROOT`] and (via `concat!`) into the recursive `fs.read` grant
//! pattern [`PROJECT_FS_PATTERN`]. The pure, fail-closed gate
//! [`is_within_project_scope`] derives from [`PROJECT_ROOT`], so the granted
//! scope and every pure allow/deny check cannot drift apart. Symlink and
//! device resolution stay with the host per security policy; this module is
//! pure and performs no I/O.

/// Literal source of [`PROJECT_ROOT`], expanded into [`PROJECT_FS_PATTERN`]
/// so the recursive grant cannot drift from the scope root.
macro_rules! project_root_literal {
    () => {
        "~/projects"
    };
}

/// Project filesystem root — single source of truth for the scope prefix.
pub const PROJECT_ROOT: &str = project_root_literal!();

/// Recursive path-glob capability pattern (`~/projects/**`) granted as
/// `fs.read:...` by first-party project surfaces. Derived from
/// [`PROJECT_ROOT`] at compile time.
pub const PROJECT_FS_PATTERN: &str = concat!(project_root_literal!(), "/**");

/// Maximum path bytes accepted by the pure project-scope gate — parser bound
/// for cwd-style paths.
pub const PROJECT_PATH_MAX_BYTES: usize = 4096;

/// Whether `path` is inside the [`PROJECT_ROOT`] isolation boundary.
///
/// Pure, bounded, fail-closed: accepts exactly `~/projects`, `~/projects/`,
/// or a descendant; rejects empty paths, paths over
/// [`PROJECT_PATH_MAX_BYTES`], NUL and control characters, `..` traversal
/// below the root, and root-prefixed siblings such as `~/projectsx`.
/// Symlink/device checks are deferred to host real-path resolution; this gate
/// rejects obvious escapes headlessly.
#[must_use]
pub fn is_within_project_scope(path: &str) -> bool {
    if path.is_empty() || path.len() > PROJECT_PATH_MAX_BYTES || path.contains('\0') {
        return false;
    }
    if path == PROJECT_ROOT {
        return true;
    }
    let Some(rest) = path.strip_prefix(PROJECT_ROOT) else {
        return false;
    };
    if !rest.starts_with('/') {
        return false;
    }
    if rest == "/" {
        return true;
    }
    !rest.contains("..") && !rest.chars().any(|c| c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_value_is_pinned_and_derived() {
        assert_eq!(PROJECT_ROOT, "~/projects");
        assert_eq!(PROJECT_FS_PATTERN, "~/projects/**");
        assert_eq!(PROJECT_PATH_MAX_BYTES, 4096);
        assert_eq!(
            PROJECT_FS_PATTERN.strip_suffix("/**"),
            Some(PROJECT_ROOT),
            "grant pattern must be derived from the single-source root"
        );
    }

    #[test]
    fn scope_accepts_root_and_descendants() {
        for allowed in [
            "~/projects",
            "~/projects/",
            "~/projects/foo",
            "~/projects/foo/bar",
            "~/projects/a b",
        ] {
            assert!(is_within_project_scope(allowed), "{allowed}");
        }
    }

    #[test]
    fn scope_fails_closed_on_escapes() {
        for denied in [
            "",
            "~/projectsx",
            "~/projectsfoo/bar",
            "~/Documents/foo",
            "/home/user/projects/foo",
            "~/projects/../etc/passwd",
            "~/projects/foo/../../etc",
            "~/projects/foo..bar",
            "~/projects/\0evil",
            "~/projects/foo\x07",
        ] {
            assert!(!is_within_project_scope(denied), "{denied}");
        }
    }

    #[test]
    fn scope_length_bound_is_exactly_4096() {
        let exact = format!(
            "~/projects/{}",
            "a".repeat(PROJECT_PATH_MAX_BYTES - "~/projects/".len())
        );
        assert_eq!(exact.len(), PROJECT_PATH_MAX_BYTES);
        assert!(is_within_project_scope(&exact));
        let over = format!("{exact}a");
        assert!(!is_within_project_scope(&over));
    }

    #[test]
    fn project_manifest_grant_matches_single_source_pattern() {
        let manifest = bitty_plugin_host::bundled::project_manifest();
        assert_eq!(manifest.capabilities.filesystem.len(), 1);
        assert_eq!(
            manifest.capabilities.filesystem[0].paths,
            vec![PROJECT_FS_PATTERN.to_string()]
        );
    }
}
