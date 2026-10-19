//! Bounded, capability-gated `bitty.fs.list` backend (CTX-1348).
//!
//! The Lua bridge validates request shape and calls
//! [`HostServices::fs_list`](bitty_lua::HostServices::fs_list); this module
//! supplies the real-filesystem resolution the accepted policy layer leaves
//! to the host I/O boundary (`bitty-plugin-host` is pure and performs no I/O).
//!
//! # Security boundary
//!
//! - **Grant gate.** Only `fs.read:` capability strings are consulted
//!   (listing is read-class); an absent grant denies with
//!   `E_FS_MISSING_GRANT`. Matching uses the accepted
//!   [`FilesystemScope`] matcher, so glob semantics, hostile-pattern
//!   rejection, and `..`/absolute rejection cannot drift from `FsGate`.
//! - **Real-path gate.** A `~`-rooted request maps to `$HOME` and the
//!   canonicalized real path must stay under the canonical literal root of
//!   the same grant (the prefix before the first glob segment). A symlink
//!   whose target leaves that root is denied; a `..` or absolute request is
//!   hostile before any filesystem call.
//! - **No ambient enumeration.** Entries outside the authorizing grant,
//!   sensitive paths without consent, hostile names, symlinks whose target
//!   leaves the grant, and non-file/non-directory entries are silently
//!   skipped, matching the RFC-0005 silent-skip listing default.
//! - **Bounded result.** At most `max_entries` (bridge cap
//!   [`FS_LIST_MAX_ENTRIES`]) entries are returned, deterministically sorted
//!   by name, and the returned name bytes never exceed [`FS_LIST_MAX_BYTES`].
//! - **Oracle-tight failures.** Typed `E_FS_*` denials carry no path or
//!   content bytes; a missing directory is `E_NOT_FOUND`; every other I/O
//!   failure fails closed as a scope denial.
//!
//! Relative grants (no `~` root) have no stable host base and therefore
//! resolve to no usable grant: the call fails closed instead of guessing a
//! working directory.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use bitty_lua::{BridgeError, FS_LIST_MAX_BYTES, FS_PATH_MAX_BYTES, validate_fs_list_max};
use bitty_plugin_host::{
    FilesystemScope, FsBridgeDenialKind, SensitivePathPolicy, is_hostile_fs_pattern,
};
use bitty_rich::ResourcePolicy;

/// Capability head that authorizes listing (`fs.read:PATTERN`).
const FS_READ_HEAD: &str = "fs.read:";

/// One visible directory entry (name plus file-kind metadata, no bytes).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FsListEntry {
    /// Logical entry path (`~/...`), never an absolute host path.
    pub name: String,
    /// Entry kind: `file` or `dir`.
    pub kind: &'static str,
}

/// Production real-filesystem backend for `bitty.fs.list`.
///
/// Construct once at the composition root with the environment-derived home
/// directory; the per-generation grant snapshot stays in `PluginServices`
/// and is passed to [`Self::list`] on every call.
#[derive(Debug, Clone)]
pub struct FsListBackend {
    home: PathBuf,
}

impl FsListBackend {
    /// Create a backend rooted at `home` for `~`-rooted requests.
    ///
    /// The home is not canonicalized here: grants whose literal root does not
    /// resolve fail closed at call time, and every real path is canonicalized
    /// before use.
    #[must_use]
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// List at most `max_entries` visible entries under `prefix`.
    ///
    /// `granted` is the calling generation's full activation grant snapshot;
    /// only `fs.read:` entries are considered. Results are deterministic
    /// (lexicographic name order), carry logical `~/...` names, and are
    /// bounded by [`FS_LIST_MAX_ENTRIES`] and [`FS_LIST_MAX_BYTES`].
    ///
    /// # Errors
    ///
    /// `E_DEF_LIMIT`/`E_DEF_INVALID` for an out-of-range page bound,
    /// `E_FS_MISSING_GRANT` when no usable read grant exists,
    /// `E_FS_HOSTILE_PATTERN` for traversal/absolute/device shapes,
    /// `E_FS_SENSITIVE_PATH` for a sensitive prefix, `E_FS_SCOPE_MISMATCH`
    /// for an out-of-grant prefix or a symlink escape, and `E_NOT_FOUND` when
    /// the granted directory does not exist. Other I/O failures fail closed
    /// as `E_FS_SCOPE_MISMATCH`.
    pub fn list(
        &self,
        granted: &[String],
        prefix: &str,
        max_entries: usize,
    ) -> Result<Vec<FsListEntry>, BridgeError> {
        validate_fs_list_max(max_entries)?;

        let policy = SensitivePathPolicy::default_policy();
        let logical = normalize_request_path(prefix)
            .ok_or_else(|| denial(FsBridgeDenialKind::HostilePattern))?;
        if is_hostile_fs_pattern(&logical) {
            return Err(denial(FsBridgeDenialKind::HostilePattern));
        }

        // Grant checks precede the sensitive-path check, matching `FsGate`:
        // an out-of-scope caller learns nothing about sensitive state.
        let scopes = self.grant_scopes(granted);
        if scopes.is_empty() {
            return Err(denial(FsBridgeDenialKind::MissingGrant));
        }
        let covering: Vec<&GrantScope> = scopes
            .iter()
            .filter(|scope| scope.scope.allows(&logical))
            .collect();
        if covering.is_empty() {
            return Err(denial(FsBridgeDenialKind::ScopeMismatch));
        }
        if policy.is_sensitive(&logical) {
            return Err(denial(FsBridgeDenialKind::SensitivePath));
        }

        let requested = self
            .real_path(&logical)
            .ok_or_else(|| denial(FsBridgeDenialKind::ScopeMismatch))?;
        let canonical = canonicalize(&requested)?;
        let authorized: Vec<&GrantScope> = covering
            .into_iter()
            .filter(|scope| scope.root_covers(&canonical))
            .collect();
        if authorized.is_empty() {
            // In-scope lexically but outside every canonical grant root:
            // a symlink escape (or a grant root that no longer resolves).
            return Err(denial(FsBridgeDenialKind::ScopeMismatch));
        }

        let dir = std::fs::read_dir(&canonical).map_err(io_error)?;

        // Deterministic, bounded selection: keep the lexicographically
        // smallest `max_entries` names without buffering an unbounded
        // directory in memory.
        let mut bounded: BTreeSet<(String, &'static str)> = BTreeSet::new();
        for entry in dir {
            let Ok(entry) = entry else {
                // A failed directory read is a fail-closed omission, never a
                // partial entry leak.
                continue;
            };
            let Ok(name) = entry.file_name().into_string() else {
                // Non-UTF-8 names cannot cross the Lua boundary intact.
                continue;
            };
            if !entry_name_ok(&name) {
                continue;
            }
            let child = format!("{logical}/{name}");
            if !authorized.iter().any(|scope| scope.scope.allows(&child)) {
                continue;
            }
            if policy.is_sensitive(&child) || is_hostile_fs_pattern(&child) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let Some(kind) = entry_kind(&entry, &file_type, &child, &authorized) else {
                continue;
            };
            bounded.insert((child, kind));
            if bounded.len() > max_entries {
                bounded.pop_last();
            }
        }

        let mut entries = Vec::new();
        let mut bytes = 0usize;
        for (name, kind) in bounded {
            let name_bytes = name.len();
            if bytes.saturating_add(name_bytes) > FS_LIST_MAX_BYTES {
                break;
            }
            bytes += name_bytes;
            entries.push(FsListEntry { name, kind });
        }
        Ok(entries)
    }

    /// Build usable `fs.read:` scopes with canonical literal roots.
    ///
    /// A grant is usable only when its pattern passes the accepted scope
    /// matcher (hostile/invalid patterns are skipped), is `~`-rooted (the
    /// only root with a stable real-path base), and its literal prefix
    /// resolves through the approved-path policy.
    fn grant_scopes(&self, granted: &[String]) -> Vec<GrantScope> {
        let mut scopes = Vec::new();
        for capability in granted {
            let Some(pattern) = capability.strip_prefix(FS_READ_HEAD) else {
                continue;
            };
            let Some(pattern) = normalize_grant_pattern(pattern) else {
                continue;
            };
            let Ok(scope) = FilesystemScope::from_patterns(std::slice::from_ref(&pattern)) else {
                continue;
            };
            let Some(root) = self.literal_root(&pattern) else {
                continue;
            };
            scopes.push(GrantScope { scope, root });
        }
        scopes
    }

    /// Canonical real root of a grant's literal prefix (`~/docs/**` ->
    /// `$HOME/docs`), through the approved-path policy.
    fn literal_root(&self, pattern: &str) -> Option<PathBuf> {
        let segments: Vec<&str> = pattern.split('/').collect();
        let literals: Vec<&str> = segments
            .iter()
            .skip(1)
            .take_while(|segment| !has_glob(segment))
            .copied()
            .collect();
        if literals.is_empty() {
            // `~/**` and other overbroad roots are already hostile upstream;
            // fail closed instead of treating the home itself as a grant.
            return None;
        }
        if literals.iter().any(|segment| segment.contains(':')) {
            // No accepted home-scoped pattern needs a drive/stream prefix.
            return None;
        }
        let mut root = self.home.clone();
        for literal in literals {
            root.push(literal);
        }
        let policy = ResourcePolicy::new(vec![root]).ok()?;
        policy.roots().first().cloned()
    }

    /// Map a normalized logical `~/...` path below the configured home.
    fn real_path(&self, logical: &str) -> Option<PathBuf> {
        let mut path = self.home.clone();
        for segment in logical.split('/').skip(1) {
            if segment.is_empty() || segment == "." || segment == ".." || segment.contains(':') {
                return None;
            }
            path.push(segment);
        }
        Some(path)
    }
}

/// One usable grant: accepted scope matcher plus canonical literal root.
struct GrantScope {
    scope: FilesystemScope,
    root: PathBuf,
}

impl GrantScope {
    fn root_covers(&self, canonical: &Path) -> bool {
        canonical == self.root || canonical.starts_with(&self.root)
    }
}

/// Normalize one request path under the accepted path rules.
///
/// Returns the logical `~/...` form only for a home-rooted path. A leading
/// empty/`.` run pins the path as relative (the accepted rule), and an
/// absolute path, `..`, `~user`, drive prefix, or unsupported relative path
/// returns `None` (fail closed).
fn normalize_request_path(path: &str) -> Option<String> {
    if path.is_empty() || path.len() > FS_PATH_MAX_BYTES || path.contains('\0') {
        return None;
    }
    if path.chars().any(char::is_control) {
        return None;
    }
    let segments: Vec<&str> = path.split(['/', '\\']).collect();
    if segments.contains(&"..") {
        return None;
    }
    let first_real = segments
        .iter()
        .position(|segment| !segment.is_empty() && *segment != ".")?;
    if first_real != 0 {
        // Leading separator or `./`: relative/absolute, never `~`-rooted.
        return None;
    }
    let normalized: Vec<&str> = segments
        .iter()
        .filter(|segment| !segment.is_empty() && **segment != ".")
        .copied()
        .collect();
    if normalized.first() != Some(&"~") {
        return None;
    }
    if normalized.iter().any(|segment| segment.contains(':')) {
        return None;
    }
    Some(normalized.join("/"))
}

/// Normalize one `fs.read:` grant pattern to `~/...` form.
fn normalize_grant_pattern(pattern: &str) -> Option<String> {
    if pattern.is_empty() || pattern.contains('\0') {
        return None;
    }
    let segments: Vec<&str> = pattern.split(['/', '\\']).collect();
    if segments.contains(&"..") {
        return None;
    }
    let first_real = segments
        .iter()
        .position(|segment| !segment.is_empty() && *segment != ".")?;
    if first_real != 0 {
        return None;
    }
    let normalized: Vec<&str> = segments
        .iter()
        .filter(|segment| !segment.is_empty() && **segment != ".")
        .copied()
        .collect();
    if normalized.first() != Some(&"~") {
        return None;
    }
    Some(normalized.join("/"))
}

/// Whether a grant segment contains glob syntax.
fn has_glob(segment: &str) -> bool {
    segment.contains(['*', '?', '[', ']', '{', '}'])
}

/// Whether one filesystem name can cross the Lua bridge intact and cannot
/// smuggle a path separator or platform prefix.
fn entry_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', ':'])
        && !name.chars().any(char::is_control)
}

/// File kind of one visible entry, following symlinks only inside the grant.
///
/// Devices, FIFOs, sockets, broken links, and symlinks whose canonical target
/// leaves every authorizing grant root are omitted.
fn entry_kind(
    entry: &std::fs::DirEntry,
    file_type: &std::fs::FileType,
    child: &str,
    authorized: &[&GrantScope],
) -> Option<&'static str> {
    if file_type.is_dir() {
        return Some("dir");
    }
    if file_type.is_file() {
        return Some("file");
    }
    if !file_type.is_symlink() {
        return None;
    }
    let target = std::fs::canonicalize(entry.path()).ok()?;
    let inside = authorized
        .iter()
        .any(|scope| scope.root_covers(&target) && scope.scope.allows(child));
    if !inside {
        return None;
    }
    let metadata = std::fs::metadata(&target).ok()?;
    if metadata.is_dir() {
        Some("dir")
    } else if metadata.is_file() {
        Some("file")
    } else {
        None
    }
}

/// Canonicalize a requested directory, keeping missing paths distinct.
fn canonicalize(path: &Path) -> Result<PathBuf, BridgeError> {
    std::fs::canonicalize(path).map_err(io_error)
}

/// Map an I/O failure to a typed bridge error without leaking host paths.
fn io_error(error: std::io::Error) -> BridgeError {
    if error.kind() == std::io::ErrorKind::NotFound {
        return BridgeError::new("io", "E_NOT_FOUND", "fs directory not found");
    }
    // Permissions, races, and other failures stay indistinguishable from a
    // scope denial: fail closed, never an existence oracle.
    denial(FsBridgeDenialKind::ScopeMismatch)
}

/// Typed, oracle-tight filesystem denial for the accepted family.
fn denial(kind: FsBridgeDenialKind) -> BridgeError {
    BridgeError::new(
        "runtime",
        kind.code(),
        format!("fs denied for family 'fs' ({})", kind.code()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_lua::FS_LIST_MAX_ENTRIES;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitty-ctx1348-fs-list-{tag}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp home");
        dir
    }

    fn touch(home: &Path, relative: &str, content: &str) {
        let path = home.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, content).expect("write fixture");
    }

    fn grant(pattern: &str) -> Vec<String> {
        vec![format!("fs.read:{pattern}")]
    }

    #[test]
    fn granted_prefix_lists_expected_entries_deterministically() {
        let home = temp_home("granted");
        touch(&home, "docs/b.txt", "b");
        touch(&home, "docs/a.txt", "a");
        touch(&home, "docs/sub/nested.txt", "nested");
        std::fs::create_dir_all(home.join("other")).expect("other dir");
        let backend = FsListBackend::new(&home);

        let entries = backend
            .list(&grant("~/docs/**"), "~/docs", 16)
            .expect("granted listing");

        // Name order, directories and files mixed; the out-of-grant `other`
        // tree is never visible.
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["~/docs/a.txt", "~/docs/b.txt", "~/docs/sub"]);
        let kinds: Vec<&str> = entries.iter().map(|entry| entry.kind).collect();
        assert_eq!(kinds, vec!["file", "file", "dir"]);
        assert!(entries.iter().all(|entry| !entry.name.contains("other")));

        // Trailing separators and `.` segments normalize to the same page.
        let again = backend
            .list(&grant("~/docs/**"), "~/docs//./", 16)
            .expect("normalized listing");
        assert_eq!(again, entries);
    }

    #[test]
    fn ungranted_prefix_and_no_grant_deny_typed() {
        let home = temp_home("ungranted");
        touch(&home, "docs/a.txt", "a");
        std::fs::create_dir_all(home.join("outside")).expect("outside dir");
        let backend = FsListBackend::new(&home);

        let error = backend
            .list(&grant("~/docs/**"), "~/outside", 16)
            .expect_err("out-of-grant prefix must deny");
        assert_eq!(error.code, "E_FS_SCOPE_MISMATCH");

        let error = backend
            .list(&[], "~/docs", 16)
            .expect_err("no grant must deny");
        assert_eq!(error.code, "E_FS_MISSING_GRANT");

        // A write grant never authorizes a listing.
        let error = backend
            .list(&["fs.write:~/docs/**".to_string()], "~/docs", 16)
            .expect_err("write grant must not list");
        assert_eq!(error.code, "E_FS_MISSING_GRANT");

        // Traversal is hostile before any filesystem call.
        let error = backend
            .list(&grant("~/docs/**"), "~/docs/../outside", 16)
            .expect_err("traversal must deny");
        assert_eq!(error.code, "E_FS_HOSTILE_PATTERN");

        // Absolute paths are never home-rooted.
        let error = backend
            .list(&grant("~/docs/**"), "/etc", 16)
            .expect_err("absolute prefix must deny");
        assert_eq!(error.code, "E_FS_HOSTILE_PATTERN");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_denied_and_not_listed() {
        let home = temp_home("symlink");
        touch(&home, "docs/inside.txt", "inside");
        let outside = temp_home("symlink-outside");
        touch(&outside, "secret.txt", "secret");
        std::os::unix::fs::symlink(&outside, home.join("docs/escape")).expect("symlink");
        std::os::unix::fs::symlink(outside.join("secret.txt"), home.join("docs/secret-link"))
            .expect("file symlink");
        let backend = FsListBackend::new(&home);

        // Resolving the symlinked directory must not escape the grant.
        let error = backend
            .list(&grant("~/docs/**"), "~/docs/escape", 16)
            .expect_err("symlink directory escape must deny");
        assert_eq!(error.code, "E_FS_SCOPE_MISMATCH");

        // The listing itself silently skips both escaping symlinks.
        let entries = backend
            .list(&grant("~/docs/**"), "~/docs", 16)
            .expect("granted listing");
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["~/docs/inside.txt"]);
        assert!(!entries.iter().any(|entry| entry.name.contains("escape")));
        assert!(
            !entries
                .iter()
                .any(|entry| entry.name.contains("secret-link"))
        );

        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn sibling_prefix_symlink_is_not_inside_the_grant() {
        // Containment is component-aware: `~/docs2` must not satisfy a
        // `~/docs` grant even though its string form starts with `docs`.
        let home = temp_home("sibling-prefix");
        touch(&home, "docs/inside.txt", "inside");
        touch(&home, "docs2/secret.txt", "secret");
        std::os::unix::fs::symlink(home.join("docs2"), home.join("docs/escape")).expect("symlink");
        let backend = FsListBackend::new(&home);

        let error = backend
            .list(&grant("~/docs/**"), "~/docs/escape", 16)
            .expect_err("sibling-prefix escape must deny");
        assert_eq!(error.code, "E_FS_SCOPE_MISMATCH");
        let entries = backend
            .list(&grant("~/docs/**"), "~/docs", 16)
            .expect("granted listing");
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["~/docs/inside.txt"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inside_grant_is_followed() {
        let home = temp_home("symlink-inside");
        touch(&home, "docs/real/file.txt", "x");
        std::os::unix::fs::symlink(home.join("docs/real"), home.join("docs/alias"))
            .expect("symlink");
        let backend = FsListBackend::new(&home);

        let entries = backend
            .list(&grant("~/docs/**"), "~/docs", 16)
            .expect("granted listing");
        assert!(
            entries
                .iter()
                .any(|entry| { entry.name == "~/docs/alias" && entry.kind == "dir" })
        );
        let nested = backend
            .list(&grant("~/docs/**"), "~/docs/alias", 16)
            .expect("inside-grant symlink listing");
        assert_eq!(
            nested
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["~/docs/alias/file.txt"]
        );
    }

    #[test]
    fn oversized_directory_is_bounded_by_entries_and_bytes() {
        let home = temp_home("oversized");
        std::fs::create_dir_all(home.join("docs")).expect("docs");
        for index in 0..(FS_LIST_MAX_ENTRIES + 8) {
            touch(&home, &format!("docs/file-{index:04}.txt"), "x");
        }
        let backend = FsListBackend::new(&home);
        let entries = backend
            .list(&grant("~/docs/**"), "~/docs", FS_LIST_MAX_ENTRIES)
            .expect("bounded listing");
        // Neither cap may be exceeded: the requested page and the bridge
        // entry ceiling, and the 8 KiB returned-name ceiling.
        assert!(entries.len() <= FS_LIST_MAX_ENTRIES);
        let total: usize = entries.iter().map(|entry| entry.name.len()).sum();
        assert!(total <= FS_LIST_MAX_BYTES, "name bytes {total}");
        // Deterministically the first page in name order.
        assert_eq!(entries[0].name, "~/docs/file-0000.txt");

        // An explicit smaller page truncates deterministically.
        let small = backend
            .list(&grant("~/docs/**"), "~/docs", 8)
            .expect("small page");
        assert_eq!(small.len(), 8);
        assert_eq!(small[0].name, "~/docs/file-0000.txt");
        assert_eq!(small[7].name, "~/docs/file-0007.txt");

        // The bridge ceiling is enforced, not silently clamped.
        let error = backend
            .list(&grant("~/docs/**"), "~/docs", FS_LIST_MAX_ENTRIES + 1)
            .expect_err("oversized page must fail");
        assert_eq!(error.code, "E_DEF_LIMIT");

        // The byte ceiling bounds the returned name bytes even below the
        // entry cap.
        let long = "x".repeat(200);
        for index in 0..64 {
            touch(&home, &format!("docs/long-{index:02}-{long}"), "x");
        }
        let entries = backend
            .list(&grant("~/docs/**"), "~/docs", FS_LIST_MAX_ENTRIES)
            .expect("byte-bounded listing");
        let total: usize = entries.iter().map(|entry| entry.name.len()).sum();
        assert!(total <= FS_LIST_MAX_BYTES, "name bytes {total}");
        assert!(!entries.is_empty());
    }

    #[test]
    fn missing_directory_is_typed_not_found() {
        let home = temp_home("missing");
        std::fs::create_dir_all(home.join("docs")).expect("docs");
        let backend = FsListBackend::new(&home);
        let error = backend
            .list(&grant("~/docs/**"), "~/docs/missing", 16)
            .expect_err("missing directory must fail");
        assert_eq!(error.code, "E_NOT_FOUND");
    }

    #[test]
    fn sensitive_paths_are_denied_or_suppressed() {
        let home = temp_home("sensitive");
        touch(&home, "docs/.env", "K=v");
        touch(&home, "docs/notes.txt", "n");
        let backend = FsListBackend::new(&home);

        let entries = backend
            .list(&grant("~/docs/**"), "~/docs", 16)
            .expect("granted listing");
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["~/docs/notes.txt"]
        );

        let error = backend
            .list(&grant("~/docs/**"), "~/docs/.env", 16)
            .expect_err("sensitive prefix must deny");
        assert_eq!(error.code, "E_FS_SENSITIVE_PATH");
    }

    #[test]
    fn relative_and_overbroad_grants_fail_closed() {
        let home = temp_home("relative");
        touch(&home, "docs/a.txt", "a");
        let backend = FsListBackend::new(&home);

        // A relative grant has no stable real-path base.
        let error = backend
            .list(&["fs.read:docs/**".to_string()], "~/docs", 16)
            .expect_err("relative grant must not map");
        assert_eq!(error.code, "E_FS_MISSING_GRANT");

        // An overbroad home grant is hostile at grant construction.
        let error = backend
            .list(&["fs.read:~/**".to_string()], "~/docs", 16)
            .expect_err("overbroad grant must not map");
        assert_eq!(error.code, "E_FS_MISSING_GRANT");

        // A malformed pattern is skipped, never widened.
        let error = backend
            .list(&["fs.read:~/docs/../**".to_string()], "~/docs", 16)
            .expect_err("traversal grant must not map");
        assert_eq!(error.code, "E_FS_MISSING_GRANT");
    }
}
