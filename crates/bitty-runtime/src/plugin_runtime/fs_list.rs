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
//! - **Symlinked grant roots fail closed.** A grant whose literal segments
//!   below the home contain a symlink is unusable (`E_FS_MISSING_GRANT`).
//!   A symlinked *home prefix* stays allowed: the home itself may be a
//!   symlink, and its canonical form is the authority. This decision is
//!   pinned by regression tests.
//! - **No ambient enumeration.** Entries outside the authorizing grant,
//!   sensitive paths without consent, hostile names, symlinks whose target
//!   leaves the grant, and non-file/non-directory entries are silently
//!   skipped, matching the RFC-0005 silent-skip listing default.
//! - **Bounded result.** At most `max_entries` (bridge cap
//!   `FS_LIST_MAX_ENTRIES`) entries are returned, deterministically sorted
//!   by name, and the returned name bytes never exceed [`FS_LIST_MAX_BYTES`].
//! - **Bounded time.** The walk checks its deadline every
//!   `SCAN_DEADLINE_CHECK_INTERVAL` entries and aborts with `E_TIMEOUT`
//!   instead of finishing an over-budget scan. Bridge calls pass the
//!   enclosing callback expiry; direct calls use the accepted RC-1 wall
//!   budget.
//! - **Oracle-tight failures.** Typed `E_FS_*` denials carry no path or
//!   content bytes. `E_NOT_FOUND` is returned only after the nearest
//!   existing ancestor has been resolved and proven inside the grant, so a
//!   missing in-grant leaf never distinguishes an out-of-grant target that
//!   exists from one that does not.
//!
//! Relative grants (no `~` root) have no stable host base and therefore
//! resolve to no usable grant: the call fails closed instead of guessing a
//! working directory.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use bitty_lua::{
    BridgeError, FS_LIST_MAX_BYTES, FS_PATH_MAX_BYTES, RC1_WALL_CLOCK_BUDGET_MS,
    validate_fs_list_max,
};
use bitty_plugin_host::{
    FilesystemScope, FsBridgeDenialKind, SensitivePathPolicy, is_hostile_fs_pattern,
};
use bitty_rich::ResourcePolicy;

/// Capability head that authorizes listing (`fs.read:PATTERN`).
const FS_READ_HEAD: &str = "fs.read:";

/// Entries between deadline checks inside one directory walk.
///
/// Checking every entry would add a clock read to the per-entry cost; every
/// 64 entries keeps the abort close to the deadline while the scan still
/// dominates the loop body.
const SCAN_DEADLINE_CHECK_INTERVAL: usize = 64;

/// Wall-clock budget for one direct [`FsListBackend::list`] call.
///
/// Reuses the accepted RC-1 per-callback wall budget; the Lua bridge passes
/// its own callback expiry through [`FsListBackend::list_with_expiry`], so
/// production calls never outlive the enclosing callback.
pub(super) const LIST_SCAN_BUDGET: Duration = Duration::from_millis(RC1_WALL_CLOCK_BUDGET_MS);

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
    /// `home` must be absolute. A relative or empty home makes every grant
    /// unusable (fail closed), because its literal roots would otherwise
    /// resolve against the process working directory.
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
    /// bounded by `FS_LIST_MAX_ENTRIES` and [`FS_LIST_MAX_BYTES`]. The
    /// walk is bounded by the accepted RC-1 wall budget; use
    /// [`Self::list_with_expiry`] to pass the enclosing callback deadline.
    ///
    /// # Errors
    ///
    /// `E_DEF_LIMIT`/`E_DEF_INVALID` for an out-of-range page bound,
    /// `E_FS_MISSING_GRANT` when no usable read grant exists,
    /// `E_FS_HOSTILE_PATTERN` for traversal/absolute/device shapes,
    /// `E_FS_SENSITIVE_PATH` for a sensitive prefix, `E_FS_SCOPE_MISMATCH`
    /// for an out-of-grant prefix or a symlink escape, `E_TIMEOUT` when the
    /// scan deadline expires, and `E_NOT_FOUND` when the granted directory
    /// does not exist *inside* the grant. Other I/O failures fail closed as
    /// `E_FS_SCOPE_MISMATCH`.
    pub fn list(
        &self,
        granted: &[String],
        prefix: &str,
        max_entries: usize,
    ) -> Result<Vec<FsListEntry>, BridgeError> {
        self.list_with_expiry(
            granted,
            prefix,
            max_entries,
            Instant::now() + LIST_SCAN_BUDGET,
        )
    }

    /// Expiry-aware [`Self::list`] for the bridge callback deadline.
    ///
    /// Same contract as [`Self::list`], except that the directory walk checks
    /// `expiry` every `SCAN_DEADLINE_CHECK_INTERVAL` entries and aborts
    /// with `E_TIMEOUT`. The deadline is checked before any per-entry
    /// symlink resolution, so one escaping link cannot extend the scan past
    /// the budget.
    pub fn list_with_expiry(
        &self,
        granted: &[String],
        prefix: &str,
        max_entries: usize,
        expiry: Instant,
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
        // Resolve the nearest existing ancestor on `NotFound` so the
        // containment check runs before `E_NOT_FOUND` is decided: a missing
        // leaf under an escaping symlink must not answer "absent" while an
        // existing leaf under the same link answers "denied".
        let (canonical, missing) = resolve_nearest_existing(&requested)?;
        let authorized: Vec<&GrantScope> = covering
            .into_iter()
            .filter(|scope| scope.root_covers(&canonical))
            .collect();
        if authorized.is_empty() {
            // In-scope lexically but outside every canonical grant root:
            // a symlink escape (or a grant root that no longer resolves).
            // Containment precedes every sensitive fact so an out-of-grant
            // caller learns nothing about the resolved target.
            return Err(denial(FsBridgeDenialKind::ScopeMismatch));
        }
        // Symlink resolution can introduce sensitive component names the
        // logical-prefix policy never sees (`.ssh`, `authorized_keys`,
        // `shadow`); deny them before any listing.
        if resolved_sensitive(&canonical) {
            return Err(denial(FsBridgeDenialKind::SensitivePath));
        }
        if missing {
            // The nearest existing ancestor is inside the grant, so the leaf
            // is a genuine in-grant absence.
            return Err(not_found());
        }

        let dir = std::fs::read_dir(&canonical).map_err(io_error)?;

        // Deterministic, bounded selection: keep the lexicographically
        // smallest `max_entries` names without buffering an unbounded
        // directory in memory.
        let mut bounded: BTreeSet<(String, &'static str)> = BTreeSet::new();
        for (index, entry) in dir.enumerate() {
            if index % SCAN_DEADLINE_CHECK_INTERVAL == 0 && Instant::now() >= expiry {
                // Abort mid-scan instead of returning after the budget: the
                // host thread must not stay blocked for the whole walk.
                return Err(BridgeError::timeout());
            }
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
            if policy.is_sensitive(&child)
                || is_hostile_fs_pattern(&child)
                || is_sensitive_component(&name)
            {
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
    ///
    /// A literal grant segment that is itself a symlink (or that passes
    /// through one below the home) makes the grant unusable: it would
    /// silently denote the link target, and `~/docs -> /` would turn a narrow
    /// grant into host-wide enumeration. The home prefix itself may be a
    /// symlink (mounted/redirected homes); its canonical form is the
    /// authority, so only segments *below* the home are rejected.
    fn literal_root(&self, pattern: &str) -> Option<PathBuf> {
        if !self.home.is_absolute() {
            // An empty or relative home would resolve the literal grant root
            // against the process working directory, so no grant rooted there
            // is usable. The composition root already rejects such a home;
            // this keeps the public backend fail-closed on its own.
            return None;
        }
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
            // `symlink_metadata` does not follow the final component, so
            // this rejects a symlinked grant root or intermediate literal
            // while allowing a symlinked home prefix above it.
            let metadata = std::fs::symlink_metadata(&root).ok()?;
            if metadata.file_type().is_symlink() {
                return None;
            }
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
/// Devices, FIFOs, sockets, broken links, sensitive targets, and symlinks
/// whose canonical target leaves every authorizing grant root are omitted.
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
    if resolved_sensitive(&target) {
        return None;
    }
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

/// Resolve `path`, reporting whether the leaf is missing.
///
/// On `NotFound`, the nearest existing ancestor is canonicalized instead so
/// the caller can apply the canonical grant-root containment check *before*
/// deciding `E_NOT_FOUND`. The returned path is therefore either the real
/// requested directory (`missing == false`) or the real nearest existing
/// ancestor (`missing == true`).
fn resolve_nearest_existing(path: &Path) -> Result<(PathBuf, bool), BridgeError> {
    match std::fs::canonicalize(path) {
        Ok(canonical) => Ok((canonical, false)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut probe = path.parent();
            while let Some(candidate) = probe {
                match std::fs::canonicalize(candidate) {
                    Ok(canonical) => return Ok((canonical, true)),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        probe = candidate.parent();
                    }
                    Err(error) => return Err(io_error(error)),
                }
            }
            // No existing ancestor (only possible when the home itself is
            // gone): fail closed without an existence signal.
            Err(denial(FsBridgeDenialKind::ScopeMismatch))
        }
        Err(error) => Err(io_error(error)),
    }
}

/// Whether any resolved path component names a sensitive location.
///
/// `SensitivePathPolicy` matches logical `~/...` paths, so it cannot see
/// names introduced by symlink resolution. The same default-deny names are
/// checked per component here, covering `authorized_keys`, `shadow`, and
/// `.ssh` reached through a resolved component.
fn resolved_sensitive(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => name.to_str().is_none_or(is_sensitive_component),
        _ => false,
    })
}

/// Default-deny component names applied to resolved paths and entry names.
///
/// Mirrors the high-value file and directory names of the accepted
/// `SensitivePathPolicy` default set; the policy still owns the full logical
/// path matcher, this predicate only extends the same posture to components
/// the logical path cannot show.
fn is_sensitive_component(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if name == ".env" || name.starts_with(".env.") {
        return true;
    }
    if name.ends_with(".pem") || name.contains("keychain") {
        return true;
    }
    matches!(
        name.as_str(),
        ".ssh"
            | ".gnupg"
            | ".aws"
            | ".kube"
            | ".azure"
            | ".npmrc"
            | ".pypirc"
            | ".netrc"
            | "credentials.toml"
            | "authorized_keys"
            | "shadow"
            | "gshadow"
            | "id_rsa"
            | "id_ed25519"
            | "id_ecdsa"
            | "id_dsa"
    )
}

/// Map an I/O failure to a typed bridge error without leaking host paths.
fn io_error(error: std::io::Error) -> BridgeError {
    if error.kind() == std::io::ErrorKind::NotFound {
        return not_found();
    }
    // Permissions, races, and other failures stay indistinguishable from a
    // scope denial: fail closed, never an existence oracle.
    denial(FsBridgeDenialKind::ScopeMismatch)
}

/// Typed in-grant absence (decided only after ancestor containment).
fn not_found() -> BridgeError {
    BridgeError::new("io", "E_NOT_FOUND", "fs directory not found")
}

/// Typed, oracle-tight filesystem denial for the accepted family.
pub(super) fn denial(kind: FsBridgeDenialKind) -> BridgeError {
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

    /// Removes a fixture tree when the test ends, including on panic.
    ///
    /// The multi-thousand-entry fixtures below would otherwise accumulate in
    /// the temp directory across runs and eventually exhaust its inode
    /// budget, which surfaces as a misleading `StorageFull` fixture error.
    struct FixtureCleanup(PathBuf);

    impl Drop for FixtureCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Fixture deadline for tests that assert behavior other than time.
    ///
    /// The production [`FsListBackend::list`] budget is the accepted RC-1
    /// 50 ms, spent on the whole call: grant resolution, canonicalization,
    /// `read_dir`, then the walk. A saturated CI runner can burn that budget
    /// before the first deadline check, so cap/containment/security tests
    /// would fail on `E_TIMEOUT` before reaching the property they assert.
    /// They therefore run under this deadline, which no fixture scan can
    /// reach, while [`large_directory_aborts_near_the_scan_deadline`] stays
    /// the only test that exercises wall-clock expiry (expired deadline plus
    /// an explicit tiny live budget).
    const FIXTURE_SCAN_DEADLINE: Duration = Duration::from_secs(60);

    /// Listing with [`FIXTURE_SCAN_DEADLINE`], i.e. every assertion is decided
    /// by entries/bytes rather than runner speed.
    ///
    /// Non-timing tests must go through this helper; calling
    /// [`FsListBackend::list`] directly reintroduces a wall-clock race against
    /// the production budget.
    fn list_with_generous_deadline(
        backend: &FsListBackend,
        granted: &[String],
        prefix: &str,
        max_entries: usize,
    ) -> Result<Vec<FsListEntry>, BridgeError> {
        backend.list_with_expiry(
            granted,
            prefix,
            max_entries,
            Instant::now() + FIXTURE_SCAN_DEADLINE,
        )
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

    /// An empty or relative home must not turn `~/docs` into `$CWD/docs`
    /// (CTX-1348 review).
    #[test]
    fn non_absolute_home_makes_every_grant_unusable() {
        for home in ["", "relative-home", "."] {
            let backend = FsListBackend::new(home);
            let error = list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs", 16)
                .expect_err("non-absolute home must fail closed");
            assert_eq!(error.code, "E_FS_MISSING_GRANT", "home {home:?}");
        }
    }

    #[test]
    fn granted_prefix_lists_expected_entries_deterministically() {
        let home = temp_home("granted");
        touch(&home, "docs/b.txt", "b");
        touch(&home, "docs/a.txt", "a");
        touch(&home, "docs/sub/nested.txt", "nested");
        std::fs::create_dir_all(home.join("other")).expect("other dir");
        let backend = FsListBackend::new(&home);

        let entries = list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs", 16)
            .expect("granted listing");

        // Name order, directories and files mixed; the out-of-grant `other`
        // tree is never visible.
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["~/docs/a.txt", "~/docs/b.txt", "~/docs/sub"]);
        let kinds: Vec<&str> = entries.iter().map(|entry| entry.kind).collect();
        assert_eq!(kinds, vec!["file", "file", "dir"]);
        assert!(entries.iter().all(|entry| !entry.name.contains("other")));

        // Trailing separators and `.` segments normalize to the same page.
        let again = list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs//./", 16)
            .expect("normalized listing");
        assert_eq!(again, entries);
    }

    #[test]
    fn ungranted_prefix_and_no_grant_deny_typed() {
        let home = temp_home("ungranted");
        touch(&home, "docs/a.txt", "a");
        std::fs::create_dir_all(home.join("outside")).expect("outside dir");
        let backend = FsListBackend::new(&home);

        let error = list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/outside", 16)
            .expect_err("out-of-grant prefix must deny");
        assert_eq!(error.code, "E_FS_SCOPE_MISMATCH");

        let error = list_with_generous_deadline(&backend, &[], "~/docs", 16)
            .expect_err("no grant must deny");
        assert_eq!(error.code, "E_FS_MISSING_GRANT");

        // A write grant never authorizes a listing.
        let error = list_with_generous_deadline(
            &backend,
            &["fs.write:~/docs/**".to_string()],
            "~/docs",
            16,
        )
        .expect_err("write grant must not list");
        assert_eq!(error.code, "E_FS_MISSING_GRANT");

        // Traversal is hostile before any filesystem call.
        let error =
            list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs/../outside", 16)
                .expect_err("traversal must deny");
        assert_eq!(error.code, "E_FS_HOSTILE_PATTERN");

        // Absolute paths are never home-rooted.
        let error = list_with_generous_deadline(&backend, &grant("~/docs/**"), "/etc", 16)
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
        let error = list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs/escape", 16)
            .expect_err("symlink directory escape must deny");
        assert_eq!(error.code, "E_FS_SCOPE_MISMATCH");

        // The listing itself silently skips both escaping symlinks.
        let entries = list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs", 16)
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

        let error = list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs/escape", 16)
            .expect_err("sibling-prefix escape must deny");
        assert_eq!(error.code, "E_FS_SCOPE_MISMATCH");
        let entries = list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs", 16)
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

        let entries = list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs", 16)
            .expect("granted listing");
        assert!(
            entries
                .iter()
                .any(|entry| { entry.name == "~/docs/alias" && entry.kind == "dir" })
        );
        let nested = list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs/alias", 16)
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
        let _cleanup = FixtureCleanup(home.clone());
        std::fs::create_dir_all(home.join("docs")).expect("docs");
        for index in 0..(FS_LIST_MAX_ENTRIES + 8) {
            touch(&home, &format!("docs/file-{index:04}.txt"), "x");
        }
        let backend = FsListBackend::new(&home);
        let entries = list_with_generous_deadline(
            &backend,
            &grant("~/docs/**"),
            "~/docs",
            FS_LIST_MAX_ENTRIES,
        )
        .expect("bounded listing");
        // Neither cap may be exceeded: the requested page and the bridge
        // entry ceiling, and the 8 KiB returned-name ceiling.
        assert!(entries.len() <= FS_LIST_MAX_ENTRIES);
        let total: usize = entries.iter().map(|entry| entry.name.len()).sum();
        assert!(total <= FS_LIST_MAX_BYTES, "name bytes {total}");
        // Deterministically the first page in name order.
        assert_eq!(entries[0].name, "~/docs/file-0000.txt");

        // An explicit smaller page truncates deterministically.
        let small = list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs", 8)
            .expect("small page");
        assert_eq!(small.len(), 8);
        assert_eq!(small[0].name, "~/docs/file-0000.txt");
        assert_eq!(small[7].name, "~/docs/file-0007.txt");

        // The bridge ceiling is enforced, not silently clamped.
        let error = list_with_generous_deadline(
            &backend,
            &grant("~/docs/**"),
            "~/docs",
            FS_LIST_MAX_ENTRIES + 1,
        )
        .expect_err("oversized page must fail");
        assert_eq!(error.code, "E_DEF_LIMIT");

        // The byte ceiling bounds the returned name bytes even below the
        // entry cap.
        let long = "x".repeat(200);
        for index in 0..64 {
            touch(&home, &format!("docs/long-{index:02}-{long}"), "x");
        }
        let entries = list_with_generous_deadline(
            &backend,
            &grant("~/docs/**"),
            "~/docs",
            FS_LIST_MAX_ENTRIES,
        )
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
        let error =
            list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs/missing", 16)
                .expect_err("missing directory must fail");
        assert_eq!(error.code, "E_NOT_FOUND");
    }

    #[cfg(unix)]
    #[test]
    fn existence_oracle_outside_the_grant_is_closed() {
        // A symlink inside the grant that leaves it must not distinguish an
        // existing outside target from a missing one: both deny.
        let home = temp_home("oracle");
        touch(&home, "docs/inside.txt", "inside");
        let outside = temp_home("oracle-outside");
        touch(&outside, "exists.txt", "x");
        std::os::unix::fs::symlink(&outside, home.join("docs/escape")).expect("symlink");
        let backend = FsListBackend::new(&home);

        let hit = list_with_generous_deadline(
            &backend,
            &grant("~/docs/**"),
            "~/docs/escape/exists.txt",
            16,
        )
        .expect_err("existing outside target must deny");
        assert_eq!(hit.code, "E_FS_SCOPE_MISMATCH");
        let miss = list_with_generous_deadline(
            &backend,
            &grant("~/docs/**"),
            "~/docs/escape/missing-xyz",
            16,
        )
        .expect_err("missing outside target must deny the same");
        assert_eq!(miss.code, "E_FS_SCOPE_MISMATCH");

        // A genuine in-grant absence stays typed: the nearest existing
        // ancestor (`~/docs`) is inside the canonical grant root.
        let in_grant =
            list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs/missing-xyz", 16)
                .expect_err("in-grant absence must stay typed");
        assert_eq!(in_grant.code, "E_NOT_FOUND");

        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn resolved_sensitive_components_are_denied() {
        // The logical prefix (`~/docs/alias/...`) never names `.ssh`,
        // `authorized_keys`, or `shadow`; resolution does.
        let home = temp_home("resolved-sensitive");
        std::fs::create_dir_all(home.join("docs/.ssh")).expect("ssh dir");
        touch(&home, "docs/.ssh/authorized_keys", "key");
        touch(&home, "docs/shadow", "x");
        touch(&home, "docs/notes.txt", "n");
        std::os::unix::fs::symlink(home.join("docs/.ssh"), home.join("docs/alias"))
            .expect("symlink");
        let backend = FsListBackend::new(&home);

        for request in [
            "~/docs/shadow",
            "~/docs/alias",
            "~/docs/alias/authorized_keys",
        ] {
            let error = list_with_generous_deadline(&backend, &grant("~/docs/**"), request, 16)
                .expect_err("sensitive resolved component must deny");
            assert_eq!(
                error.code, "E_FS_SENSITIVE_PATH",
                "request {request} must be sensitive"
            );
        }

        // Entries naming sensitive locations are suppressed from a page,
        // including the resolved symlink alias.
        let entries = list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs", 16)
            .expect("granted listing");
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["~/docs/notes.txt"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn out_of_grant_sensitive_target_denies_as_scope_mismatch() {
        // Canonical containment must be decided before any sensitivity fact:
        // an escaping symlink whose target happens to be `~/.ssh` must answer
        // exactly like any other out-of-grant target, so a plugin cannot use
        // the denial code to probe the resolved target. An in-grant
        // sensitive path keeps its own typed denial for contrast.
        let home = temp_home("ordering");
        touch(&home, "docs/inside.txt", "x");
        touch(&home, ".ssh/authorized_keys", "k");
        touch(&home, "docs/shadow", "x");
        std::os::unix::fs::symlink(home.join(".ssh"), home.join("docs/escape")).expect("symlink");
        let backend = FsListBackend::new(&home);

        for request in [
            "~/docs/escape",
            "~/docs/escape/authorized_keys",
            "~/docs/escape/shadow",
            "~/docs/escape/missing",
        ] {
            let error = list_with_generous_deadline(&backend, &grant("~/docs/**"), request, 16)
                .expect_err("out-of-grant target must deny");
            assert_eq!(
                error.code, "E_FS_SCOPE_MISMATCH",
                "request {request} must not reveal the target's sensitivity"
            );
        }

        let error = list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs/shadow", 16)
            .expect_err("in-grant sensitive path must deny");
        assert_eq!(error.code, "E_FS_SENSITIVE_PATH");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_grant_root_fails_closed() {
        // A grant root that is itself a symlink silently changes meaning
        // (`~/docs -> /` would list the host); it is rejected at grant
        // construction time.
        let home = temp_home("root-symlink");
        let outside = temp_home("root-symlink-outside");
        touch(&outside, "secret.txt", "s");
        std::os::unix::fs::symlink(&outside, home.join("docs")).expect("root symlink");
        let backend = FsListBackend::new(&home);

        let error = list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs", 16)
            .expect_err("symlinked grant root must not resolve");
        assert_eq!(error.code, "E_FS_MISSING_GRANT");

        // The same applies to an intermediate literal segment. The target
        // tree must exist, otherwise the grant would be rejected for a
        // missing directory and the per-segment symlink check itself would
        // go untested.
        std::fs::create_dir_all(home.join("plain")).expect("plain");
        std::fs::create_dir_all(outside.join("docs")).expect("outside docs target");
        std::os::unix::fs::symlink(&outside, home.join("link")).expect("segment symlink");
        let error =
            list_with_generous_deadline(&backend, &grant("~/link/docs/**"), "~/link/docs", 16)
                .expect_err("symlinked intermediate segment must not resolve");
        assert_eq!(error.code, "E_FS_MISSING_GRANT");

        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_home_prefix_still_resolves() {
        // A mounted/redirected home is a symlink; the canonical home is the
        // authority, so legitimate listings keep working. Removing the
        // grant-root canonicalization makes this test deny.
        let real_home = temp_home("home-real");
        touch(&real_home, "docs/a.txt", "a");
        let link = std::env::temp_dir().join(format!(
            "bitty-ctx1348-home-link-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .subsec_nanos()
        ));
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real_home, &link).expect("home symlink");
        let backend = FsListBackend::new(&link);

        let entries = list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs", 16)
            .expect("symlinked home must resolve");
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["~/docs/a.txt"]
        );

        let _ = std::fs::remove_file(&link);
    }

    #[cfg(unix)]
    #[test]
    fn star_grant_does_not_list_grandchildren() {
        // `fs.read:~/docs/*` authorizes only immediate children. The
        // per-entry scope filter must keep a request for an in-scope
        // directory from widening by one level.
        let home = temp_home("star");
        touch(&home, "docs/sub/file.txt", "x");
        let backend = FsListBackend::new(&home);

        let entries = list_with_generous_deadline(&backend, &grant("~/docs/*"), "~/docs/sub", 16)
            .expect("in-scope directory lists");
        assert_eq!(entries, Vec::new());
    }

    #[cfg(unix)]
    #[test]
    fn hostile_entry_names_cannot_cross_the_bridge() {
        // Separators, colon/stream prefixes, and control bytes are legal
        // Linux file names but cannot cross the Lua bridge intact.
        let home = temp_home("entry-names");
        std::fs::create_dir_all(home.join("docs")).expect("docs");
        for name in ["ok.txt", "a:b", "a\\b", "bad\nname", "CON", "NUL.txt"] {
            std::fs::write(home.join("docs").join(name), "x").expect("entry");
        }
        let backend = FsListBackend::new(&home);

        let entries = list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs", 16)
            .expect("granted listing");
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["~/docs/ok.txt"]
        );
    }

    #[test]
    fn large_directory_aborts_near_the_scan_deadline() {
        let home = temp_home("deadline");
        let _cleanup = FixtureCleanup(home.clone());
        std::fs::create_dir_all(home.join("docs")).expect("docs");
        for index in 0..20_000 {
            std::fs::File::create(home.join(format!("docs/f{index:05}"))).expect("entry");
        }
        let backend = FsListBackend::new(&home);

        // Guard the production side of the split: the default direct-call
        // budget must stay the accepted RC-1 wall clock, so cap tests opt out
        // of the deadline instead of the backend loosening it.
        assert_eq!(
            LIST_SCAN_BUDGET,
            Duration::from_millis(bitty_lua::RC1_WALL_CLOCK_BUDGET_MS)
        );

        // A past deadline aborts deterministically at the first check.
        let error = backend
            .list_with_expiry(
                &grant("~/docs/**"),
                "~/docs",
                16,
                Instant::now() - Duration::from_millis(1),
            )
            .expect_err("expired deadline must abort");
        assert_eq!(error.code, "E_TIMEOUT");

        // A short live budget aborts mid-walk instead of finishing the scan.
        let started = Instant::now();
        let error = backend
            .list_with_expiry(
                &grant("~/docs/**"),
                "~/docs",
                16,
                started + Duration::from_millis(2),
            )
            .expect_err("over-budget scan must abort");
        let elapsed = started.elapsed();
        assert_eq!(error.code, "E_TIMEOUT");
        assert!(
            elapsed < Duration::from_secs(1),
            "scan must abort near the budget, took {elapsed:?}"
        );
    }

    #[test]
    fn sensitive_paths_are_denied_or_suppressed() {
        let home = temp_home("sensitive");
        touch(&home, "docs/.env", "K=v");
        touch(&home, "docs/notes.txt", "n");
        let backend = FsListBackend::new(&home);

        let entries = list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs", 16)
            .expect("granted listing");
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["~/docs/notes.txt"]
        );

        let error = list_with_generous_deadline(&backend, &grant("~/docs/**"), "~/docs/.env", 16)
            .expect_err("sensitive prefix must deny");
        assert_eq!(error.code, "E_FS_SENSITIVE_PATH");
    }

    #[test]
    fn relative_and_overbroad_grants_fail_closed() {
        let home = temp_home("relative");
        touch(&home, "docs/a.txt", "a");
        let backend = FsListBackend::new(&home);

        // A relative grant has no stable real-path base.
        let error =
            list_with_generous_deadline(&backend, &["fs.read:docs/**".to_string()], "~/docs", 16)
                .expect_err("relative grant must not map");
        assert_eq!(error.code, "E_FS_MISSING_GRANT");

        // An overbroad home grant is hostile at grant construction.
        let error =
            list_with_generous_deadline(&backend, &["fs.read:~/**".to_string()], "~/docs", 16)
                .expect_err("overbroad grant must not map");
        assert_eq!(error.code, "E_FS_MISSING_GRANT");

        // A malformed pattern is skipped, never widened.
        let error = list_with_generous_deadline(
            &backend,
            &["fs.read:~/docs/../**".to_string()],
            "~/docs",
            16,
        )
        .expect_err("traversal grant must not map");
        assert_eq!(error.code, "E_FS_MISSING_GRANT");
    }
}
