//! Deny-by-default local resource loader (P0-AC-005/006, R-003).
//!
//! Headless, bounded, deterministic, `forbid(unsafe_code)`.
//!
//! Implements the R-003 file-access defense per
//! `rich-presentation-rfc.md` §Resource loader and
//! `p0-acceptance-criteria.md` P0-AC-005/006:
//!
//! - deny-by-default: no filesystem access unless an explicit
//!   `ResourcePolicy` with approved canonical roots allows it;
//! - regular-file check via `metadata.is_file()`;
//! - approved-path prefix check after symlink canonicalization;
//! - path-traversal (`..`) rejection before canonicalization;
//! - forbidden prefixes `/proc`, `/sys`, `/dev` (procfs/sysfs/devfs)
//!   rejected on both raw and canonical paths;
//! - non-regular files (directories, FIFOs) denied;
//! - device and socket nodes denied via `FileTypeExt` (Unix);
//! - zero delete primitives reachable from protocol input
//!   (no filesystem removal; see `protocol_has_zero_delete_primitives`).
//!
//! All checks are bounded (`MAX_PATH_LEN`, `MAX_ROOTS`) and
//! deterministic for fixed inputs. No allocation beyond bounded
//! `PathBuf`/`String` and no ambient authority.

#![forbid(unsafe_code)]

use std::path::{Component, Path, PathBuf};

/// Max path length in bytes (bounded).
pub const MAX_PATH_LEN: usize = 4096;

/// Max approved roots (bounded).
pub const MAX_ROOTS: usize = 16;

/// Forbidden prefixes — procfs / sysfs / devfs.
const FORBIDDEN_PREFIXES: &[&str] = &["/proc", "/sys", "/dev"];

/// Typed denial; every variant is `deny` — no allow-by-default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceError {
    /// Empty path.
    EmptyPath,
    /// Path too long.
    TooLong { len: usize, cap: usize },
    /// Null byte in path.
    NullByte,
    /// Path contains `..` traversal.
    PathTraversal { path: String },
    /// Raw or canonical path under forbidden prefix.
    ForbiddenPrefix { prefix: String, path: String },
    /// Not a regular file (`is_file() == false`).
    NotRegularFile { path: String },
    /// Device node (char/block) — Unix only; maps to forbidden on non-Unix via prefix.
    DeviceDenied { path: String },
    /// Socket node.
    SocketDenied { path: String },
    /// FIFO node.
    FifoDenied { path: String },
    /// Canonical path outside all approved roots (symlink escape or
    /// approved-path policy violation).
    OutsideApprovedRoot { path: String, canonical: String },
    /// Symlink escape explicitly (canonical outside approved root
    /// while raw path appeared inside).
    SymlinkEscape { path: String, canonical: String },
    /// I/O error (e.g., canonicalize failed, metadata failed) — denied closed.
    Io { path: String, detail: String },
}

impl std::fmt::Display for ResourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPath => write!(f, "empty path denied"),
            Self::TooLong { len, cap } => write!(f, "path too long: {len} > {cap}"),
            Self::NullByte => write!(f, "null byte in path denied"),
            Self::PathTraversal { path } => write!(f, "path traversal denied: {path}"),
            Self::ForbiddenPrefix { prefix, path } => {
                write!(f, "forbidden prefix {prefix} denied: {path}")
            }
            Self::NotRegularFile { path } => write!(f, "not a regular file denied: {path}"),
            Self::DeviceDenied { path } => write!(f, "device node denied: {path}"),
            Self::SocketDenied { path } => write!(f, "socket node denied: {path}"),
            Self::FifoDenied { path } => write!(f, "fifo node denied: {path}"),
            Self::OutsideApprovedRoot { path, canonical } => {
                write!(f, "outside approved root denied: {path} -> {canonical}")
            }
            Self::SymlinkEscape { path, canonical } => {
                write!(f, "symlink escape denied: {path} -> {canonical}")
            }
            Self::Io { path, detail } => write!(f, "i/o denied for {path}: {detail}"),
        }
    }
}

impl std::error::Error for ResourceError {}

/// Approved-path policy — deny-by-default.
///
/// An empty policy denies every path. Otherwise a canonical path
/// must be a prefix-descendant of at least one approved root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourcePolicy {
    roots: Vec<PathBuf>,
}

impl ResourcePolicy {
    /// Creates a policy from `roots`, canonicalized fail-closed.
    ///
    /// Bounded: at most `MAX_ROOTS` roots.
    /// Returns `TooLong` if over cap.
    /// Fail-closed per-root validation (CR-RICH-02):
    /// - empty path -> `EmptyPath` (an empty root would grant
    ///   universal read via `Path::starts_with("")`);
    /// - over `MAX_PATH_LEN` -> `TooLong`;
    /// - null byte -> `NullByte`;
    /// - non-absolute -> `OutsideApprovedRoot`;
    /// - `..` component -> `PathTraversal`;
    /// - forbidden prefix (`/proc`, `/sys`, `/dev`) on raw or
    ///   canonical path -> `ForbiddenPrefix`;
    /// - un-canonicalizable (missing, dangling, I/O error) -> `Io`.
    ///
    /// On success stores canonical absolute roots so `is_allowed`
    /// is a pure prefix check with no filesystem I/O.
    ///
    /// An empty `roots` vec is allowed and denies all (see `deny_all`).
    pub fn new(roots: Vec<PathBuf>) -> Result<Self, ResourceError> {
        if roots.len() > MAX_ROOTS {
            return Err(ResourceError::TooLong {
                len: roots.len(),
                cap: MAX_ROOTS,
            });
        }
        let mut canon_roots = Vec::with_capacity(roots.len());
        for root in &roots {
            if root.as_os_str().is_empty() {
                return Err(ResourceError::EmptyPath);
            }
            let len = root.as_os_str().len();
            if len > MAX_PATH_LEN {
                return Err(ResourceError::TooLong {
                    len,
                    cap: MAX_PATH_LEN,
                });
            }
            if contains_null_byte(root) {
                return Err(ResourceError::NullByte);
            }
            if !root.is_absolute() {
                let s = root.display().to_string();
                return Err(ResourceError::OutsideApprovedRoot {
                    path: s.clone(),
                    canonical: s,
                });
            }
            for comp in root.components() {
                if matches!(comp, Component::ParentDir) {
                    return Err(ResourceError::PathTraversal {
                        path: root.display().to_string(),
                    });
                }
            }
            if let Some(prefix) = is_forbidden_prefix(root) {
                return Err(ResourceError::ForbiddenPrefix {
                    prefix,
                    path: root.display().to_string(),
                });
            }
            let canon = root.canonicalize().map_err(|e| ResourceError::Io {
                path: root.display().to_string(),
                detail: e.to_string(),
            })?;
            if let Some(prefix) = is_forbidden_prefix(&canon) {
                return Err(ResourceError::ForbiddenPrefix {
                    prefix,
                    path: canon.display().to_string(),
                });
            }
            canon_roots.push(canon);
        }
        Ok(Self { roots: canon_roots })
    }

    /// Deny-by-default empty policy (denies all).
    #[must_use]
    pub fn deny_all() -> Self {
        Self { roots: Vec::new() }
    }

    /// Whether `canonical` is inside any approved root.
    ///
    /// Deny-by-default: empty policy denies all. Invalid roots
    /// (empty, relative) are skipped fail-closed so a stored empty
    /// root can never grant universal read via
    /// `Path::starts_with("")` (CR-RICH-02 defense-in-depth;
    /// `new()` already rejects such roots).
    #[must_use]
    pub fn is_allowed(&self, canonical: &Path) -> bool {
        if self.roots.is_empty() {
            return false;
        }
        if canonical.as_os_str().is_empty() || !canonical.is_absolute() {
            return false;
        }
        for root in &self.roots {
            if root.as_os_str().is_empty() || !root.is_absolute() {
                continue;
            }
            if canonical.starts_with(root) {
                return true;
            }
        }
        false
    }

    /// Roots slice (canonical absolute).
    #[must_use]
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }
}

fn is_forbidden_prefix(path: &Path) -> Option<String> {
    let s = path.to_string_lossy();
    // Exact match or prefix with `/`
    for prefix in FORBIDDEN_PREFIXES {
        if s == *prefix || s.starts_with(&format!("{prefix}/")) {
            return Some((*prefix).to_string());
        }
    }
    None
}

fn contains_null_byte(path: &Path) -> bool {
    // OsStr to bytes via lossy may hide null, so check via to_string_lossy and raw bytes
    let s = path.as_os_str().to_string_lossy();
    s.contains('\0')
}

/// Validates `path` against `policy` with deny-by-default checks.
///
/// Order (bounded, deterministic):
/// 1. empty / length / null byte / `..` traversal
/// 2. forbidden prefix on raw path (`/proc`, `/sys`, `/dev`)
/// 3. metadata regular-file + device/socket/fifo deny
/// 4. canonicalize + forbidden prefix on canonical
/// 5. approved-path prefix check (symlink escape)
///
/// On success returns the canonical `PathBuf` that is guaranteed to be
/// a regular file under an approved root.
pub fn validate_resource_path(
    path: &Path,
    policy: &ResourcePolicy,
) -> Result<PathBuf, ResourceError> {
    // 1. Bounded basic checks
    if path.as_os_str().is_empty() {
        return Err(ResourceError::EmptyPath);
    }
    let len = path.as_os_str().len();
    if len > MAX_PATH_LEN {
        return Err(ResourceError::TooLong {
            len,
            cap: MAX_PATH_LEN,
        });
    }
    if contains_null_byte(path) {
        return Err(ResourceError::NullByte);
    }
    for comp in path.components() {
        if matches!(comp, Component::ParentDir) {
            return Err(ResourceError::PathTraversal {
                path: path.display().to_string(),
            });
        }
    }

    // 2. Forbidden prefix on raw
    if let Some(prefix) = is_forbidden_prefix(path) {
        return Err(ResourceError::ForbiddenPrefix {
            prefix,
            path: path.display().to_string(),
        });
    }

    // 3. Metadata checks — regular file + device/socket/fifo
    #[cfg(unix)]
    let meta = std::fs::symlink_metadata(path).map_err(|e| ResourceError::Io {
        path: path.display().to_string(),
        detail: e.to_string(),
    })?;

    // If symlink, we still need to check its target via canonicalization later,
    // but we also check the symlink itself is not a forbidden type. Use metadata (follow)
    // for device/socket checks on the target.
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        let ft = meta.file_type();
        // Check device/socket/fifo on the symlink target via `metadata` (follow)
        // if symlink_metadata says symlink, also check the target's type.
        let target_ft = if ft.is_symlink() {
            match std::fs::metadata(path) {
                Ok(m) => m.file_type(),
                Err(_) => ft,
            }
        } else {
            ft
        };
        if target_ft.is_char_device() || target_ft.is_block_device() {
            return Err(ResourceError::DeviceDenied {
                path: path.display().to_string(),
            });
        }
        if target_ft.is_socket() {
            return Err(ResourceError::SocketDenied {
                path: path.display().to_string(),
            });
        }
        if target_ft.is_fifo() {
            return Err(ResourceError::FifoDenied {
                path: path.display().to_string(),
            });
        }
    }

    // Regular file check (follows symlink to target)
    let target_meta = std::fs::metadata(path).map_err(|e| ResourceError::Io {
        path: path.display().to_string(),
        detail: e.to_string(),
    })?;
    if !target_meta.is_file() {
        return Err(ResourceError::NotRegularFile {
            path: path.display().to_string(),
        });
    }

    // 4. Canonicalize (resolves symlinks, `..`, etc.)
    let canonical = path.canonicalize().map_err(|e| ResourceError::Io {
        path: path.display().to_string(),
        detail: e.to_string(),
    })?;

    // Forbidden prefix on canonical (second layer — symlink could point to /proc)
    if let Some(prefix) = is_forbidden_prefix(&canonical) {
        return Err(ResourceError::ForbiddenPrefix {
            prefix,
            path: canonical.display().to_string(),
        });
    }

    // 5. Approved-path check
    if !policy.is_allowed(&canonical) {
        // Distinguish symlink escape vs plain outside: if raw path was inside a root
        // but canonical is outside, it's a symlink escape. Heuristic: check if raw
        // path (or its parent) starts with a root.
        let raw_inside = policy.roots.iter().any(|r| path.starts_with(r));
        if raw_inside {
            return Err(ResourceError::SymlinkEscape {
                path: path.display().to_string(),
                canonical: canonical.display().to_string(),
            });
        }
        return Err(ResourceError::OutsideApprovedRoot {
            path: path.display().to_string(),
            canonical: canonical.display().to_string(),
        });
    }

    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(suffix: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("bitty-r003-rich-{}-{}", suffix, std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        base
    }

    fn write_regular_file(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let p = dir.join(name);
        let _ = std::fs::create_dir_all(dir);
        std::fs::write(&p, content).expect("write temp file");
        p
    }

    #[test]
    fn devices_denied() {
        let policy = ResourcePolicy::deny_all();
        let err = validate_resource_path(Path::new("/dev/null"), &policy).unwrap_err();
        assert!(
            matches!(
                err,
                ResourceError::ForbiddenPrefix { .. } | ResourceError::DeviceDenied { .. }
            ),
            "expected device denied, got {err:?}"
        );
        // Also /dev/zero
        let err2 = validate_resource_path(Path::new("/dev/zero"), &policy).unwrap_err();
        assert!(matches!(
            err2,
            ResourceError::ForbiddenPrefix { .. } | ResourceError::DeviceDenied { .. }
        ));
    }

    #[test]
    fn dev_denied() {
        let policy = ResourcePolicy::deny_all();
        for raw in ["/dev", "/dev/", "/dev/urandom", "/dev/sda1"] {
            let err = validate_resource_path(Path::new(raw), &policy).unwrap_err();
            assert!(
                matches!(
                    err,
                    ResourceError::ForbiddenPrefix { .. }
                        | ResourceError::DeviceDenied { .. }
                        | ResourceError::Io { .. }
                        | ResourceError::NotRegularFile { .. }
                ),
                "dev denied failed for {raw}: {err:?}"
            );
        }
    }

    #[test]
    fn procfs_denied() {
        let policy = ResourcePolicy::deny_all();
        for raw in ["/proc", "/proc/self/mem", "/proc/sys/kernel/hostname"] {
            let err = validate_resource_path(Path::new(raw), &policy).unwrap_err();
            assert!(
                matches!(
                    err,
                    ResourceError::ForbiddenPrefix { .. }
                        | ResourceError::NotRegularFile { .. }
                        | ResourceError::Io { .. }
                ),
                "procfs denied failed for {raw}: {err:?}"
            );
            // Ensure it's not Ok
            assert!(
                matches!(err, ResourceError::ForbiddenPrefix { .. }),
                "procfs must be ForbiddenPrefix, got {err:?}"
            );
        }
    }

    #[test]
    fn sysfs_denied() {
        let policy = ResourcePolicy::deny_all();
        for raw in ["/sys", "/sys/kernel", "/sys/devices"] {
            let err = validate_resource_path(Path::new(raw), &policy).unwrap_err();
            assert!(
                matches!(err, ResourceError::ForbiddenPrefix { .. }),
                "sysfs denied failed for {raw}: {err:?}"
            );
        }
    }

    #[test]
    fn sockets_denied() {
        // Create a real Unix socket inside an approved root — must still be denied
        // because it's not a regular file (is_socket).
        #[cfg(unix)]
        {
            // macOS SUN_LEN is 104 bytes; temp_dir paths on darwin runners can be
            // ~60 chars, so use a short leaf name to keep the full path < 104.
            let root = temp_root("sk");
            let policy = ResourcePolicy::new(vec![root.clone()]).unwrap();
            let pid = std::process::id();
            let sock_path = root.join(format!("s{pid}.sock"));
            let listener = match std::os::unix::net::UnixListener::bind(&sock_path) {
                Ok(l) => l,
                Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
                    // SUN_LEN overflow on this host — use a macOS-friendly short path.
                    let short_root = PathBuf::from(format!("/tmp/b-sk-{pid}"));
                    let _ = std::fs::create_dir_all(&short_root);
                    let short_path = short_root.join("s.sock");
                    let _ = std::fs::remove_file(&short_path);
                    let _l = std::os::unix::net::UnixListener::bind(&short_path)
                        .expect("bind unix socket short");
                    // Validate against the short root policy.
                    let short_policy = ResourcePolicy::new(vec![short_root.clone()]).unwrap();
                    let err = validate_resource_path(&short_path, &short_policy).unwrap_err();
                    assert!(
                        matches!(
                            err,
                            ResourceError::SocketDenied { .. }
                                | ResourceError::NotRegularFile { .. }
                        ),
                        "socket denied failed: {err:?}"
                    );
                    return;
                }
                Err(e) => panic!("bind unix socket: {e:?}"),
            };
            let err = validate_resource_path(&sock_path, &policy).unwrap_err();
            assert!(
                matches!(
                    err,
                    ResourceError::SocketDenied { .. } | ResourceError::NotRegularFile { .. }
                ),
                "socket denied failed: {err:?}"
            );
            drop(listener);
        }
        #[cfg(not(unix))]
        {
            let policy = ResourcePolicy::deny_all();
            let err = validate_resource_path(Path::new("/tmp/fake.sock"), &policy).unwrap_err();
            assert!(matches!(
                err,
                ResourceError::Io { .. } | ResourceError::NotRegularFile { .. }
            ));
        }
    }

    #[test]
    fn symlink_escape_denied() {
        let approved = temp_root("symlink-approved");
        let outside = temp_root("symlink-outside");
        let _ = std::fs::create_dir_all(&approved);
        let _ = std::fs::create_dir_all(&outside);
        #[cfg(unix)]
        let outside_file = write_regular_file(&outside, "secret.txt", b"secret");
        #[cfg(unix)]
        let link_path = approved.join("escapes-link");
        #[cfg(unix)]
        {
            // Fresh per-pid approved dir; links are unique per run so no stale file to prune.
            std::os::unix::fs::symlink(&outside_file, &link_path).expect("symlink");
            let policy = ResourcePolicy::new(vec![approved.clone()]).unwrap();
            let err = validate_resource_path(&link_path, &policy).unwrap_err();
            assert!(
                matches!(
                    err,
                    ResourceError::SymlinkEscape { .. }
                        | ResourceError::OutsideApprovedRoot { .. }
                        | ResourceError::ForbiddenPrefix { .. }
                ),
                "symlink escape must be denied, got {err:?}"
            );
            // Also test symlink to /etc/passwd
            let link2 = approved.join("link-etc");
            let target = PathBuf::from("/etc/passwd");
            if target.exists() {
                std::os::unix::fs::symlink(&target, &link2).expect("symlink etc");
                let err2 = validate_resource_path(&link2, &policy).unwrap_err();
                assert!(
                    matches!(
                        err2,
                        ResourceError::SymlinkEscape { .. }
                            | ResourceError::OutsideApprovedRoot { .. }
                    ),
                    "symlink to /etc must be denied, got {err2:?}"
                );
            }
        }
    }

    #[test]
    fn non_regular_denied() {
        let root = temp_root("nonregular");
        let _ = std::fs::create_dir_all(&root);
        let policy = ResourcePolicy::new(vec![root.clone()]).unwrap();
        // Directory itself is not a regular file
        let err = validate_resource_path(&root, &policy).unwrap_err();
        assert!(
            matches!(err, ResourceError::NotRegularFile { .. }),
            "dir must be denied, got {err:?}"
        );
        // Subdirectory
        let sub = root.join("subdir");
        let _ = std::fs::create_dir_all(&sub);
        let err2 = validate_resource_path(&sub, &policy).unwrap_err();
        assert!(
            matches!(err2, ResourceError::NotRegularFile { .. }),
            "subdir must be denied, got {err2:?}"
        );
    }

    #[test]
    fn approved_path_policy() {
        let approved = temp_root("approved-policy");
        let outside = temp_root("outside-policy");
        let _ = std::fs::create_dir_all(&approved);
        let _ = std::fs::create_dir_all(&outside);
        let inside_file = write_regular_file(&approved, "inside.txt", b"inside");
        let outside_file = write_regular_file(&outside, "outside.txt", b"outside");
        let policy = ResourcePolicy::new(vec![approved.clone()]).unwrap();

        // Inside approved root must succeed and return canonical
        let ok = validate_resource_path(&inside_file, &policy)
            .expect("inside approved root should be allowed");
        assert!(ok.starts_with(approved.canonicalize().unwrap_or(approved.clone())));

        // Outside must be denied
        let err = validate_resource_path(&outside_file, &policy).unwrap_err();
        assert!(
            matches!(err, ResourceError::OutsideApprovedRoot { .. }),
            "outside approved root must be denied, got {err:?}"
        );

        // Empty policy denies all even for inside file
        let deny_all = ResourcePolicy::deny_all();
        let err2 = validate_resource_path(&inside_file, &deny_all).unwrap_err();
        assert!(
            matches!(
                err2,
                ResourceError::OutsideApprovedRoot { .. } | ResourceError::SymlinkEscape { .. }
            ),
            "empty policy must deny all, got {err2:?}"
        );

        // Path traversal component denied before canonical
        let traversal = approved.join("../outside-policy/outside.txt");
        // This path contains `..`
        let err3 = validate_resource_path(Path::new(&traversal), &policy).unwrap_err();
        assert!(
            matches!(err3, ResourceError::PathTraversal { .. }),
            "path traversal must be denied, got {err3:?}"
        );
    }

    #[test]
    fn protocol_has_zero_delete_primitives() {
        // Exhaustive grep: no filesystem delete primitives reachable from protocol.
        // We prove it by scanning the production source (before #[cfg(test)]) for
        // forbidden substrings. Test-only cleanup via filesystem removal would be inside
        // the `#[cfg(test)]` region, so we slice before that marker.
        let src = include_str!("loader.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        let forbidden = ["remove_file", "remove_dir", "Unlink"];
        for pat in forbidden {
            assert!(
                !prod.contains(pat),
                "loader production code must not contain delete primitive {}",
                pat
            );
        }
        // Also ensure image store production code has zero delete primitives
        let img_src = include_str!("image.rs");
        let img_prod = img_src.split("#[cfg(test)]").next().unwrap_or(img_src);
        for pat in forbidden {
            assert!(
                !img_prod.contains(pat),
                "image store production code must not contain delete primitive {}",
                pat
            );
        }
        // `delete` as a standalone word is not forbidden because `delete` appears
        // in comments about deletion semantics for ImageStore::remove (in-memory only).
        // The filesystem delete primitives above are the normative check per P0-AC-006.
    }

    #[test]
    fn bounded_and_deterministic() {
        // Same inputs yield same outputs; no panics, no allocation beyond bounds.
        let root = temp_root("bounded");
        let _ = std::fs::create_dir_all(&root);
        let file = write_regular_file(&root, "a.txt", b"hello");
        let policy = ResourcePolicy::new(vec![root.clone()]).unwrap();
        let first = validate_resource_path(&file, &policy).unwrap();
        let second = validate_resource_path(&file, &policy).unwrap();
        assert_eq!(first, second);

        // Too long path denied
        let long = "a".repeat(MAX_PATH_LEN + 1);
        let err = validate_resource_path(Path::new(&long), &policy).unwrap_err();
        assert!(matches!(err, ResourceError::TooLong { .. }));

        // Empty denied
        let err2 = validate_resource_path(Path::new(""), &policy).unwrap_err();
        assert!(matches!(err2, ResourceError::EmptyPath));

        // Too many roots denied
        let many: Vec<PathBuf> = (0..MAX_ROOTS + 1)
            .map(|i| PathBuf::from(format!("/tmp/root{i}")))
            .collect();
        let err3 = ResourcePolicy::new(many).unwrap_err();
        assert!(matches!(err3, ResourceError::TooLong { .. }));
    }

    #[test]
    fn cr_rich_02_empty_root_rejected() {
        // Primitive: empty root grants universal read via `starts_with("")`.
        assert!(
            Path::new("/etc/passwd").starts_with(Path::new("")),
            "empty-root universal-read primitive must hold for this regression to be meaningful"
        );
        let err = ResourcePolicy::new(vec![PathBuf::from("")]).unwrap_err();
        assert!(
            matches!(err, ResourceError::EmptyPath),
            "empty root must be rejected fail-closed, got {err:?}"
        );
        // Mixed valid + empty still fails closed.
        let valid = temp_root("cr-rich-02-empty-mixed");
        let err2 = ResourcePolicy::new(vec![valid, PathBuf::from("")]).unwrap_err();
        assert!(
            matches!(err2, ResourceError::EmptyPath),
            "mixed empty root must be rejected, got {err2:?}"
        );
        // Zero roots (empty vec) is allowed and denies all — distinct from
        // one empty-string root.
        let empty_vec = ResourcePolicy::new(Vec::new()).expect("empty vec denies all");
        assert!(empty_vec.roots().is_empty());
        assert!(!empty_vec.is_allowed(Path::new("/etc/passwd")));
    }

    #[test]
    fn cr_rich_02_relative_root_rejected() {
        for raw in ["relative/root", "a/b", "./tmp", "tmp"] {
            let err = ResourcePolicy::new(vec![PathBuf::from(raw)]).unwrap_err();
            assert!(
                matches!(err, ResourceError::OutsideApprovedRoot { .. }),
                "relative root {raw} must be rejected, got {err:?}"
            );
        }
    }

    #[test]
    fn cr_rich_02_traversal_root_rejected() {
        let pid = std::process::id();
        let traversal = PathBuf::from(format!("/tmp/bitty-cr-rich-02-{pid}/../escape"));
        let err = ResourcePolicy::new(vec![traversal]).unwrap_err();
        assert!(
            matches!(err, ResourceError::PathTraversal { .. }),
            "traversal root must be rejected, got {err:?}"
        );
    }

    #[test]
    fn cr_rich_02_uncanonicalizable_root_rejected() {
        let pid = std::process::id();
        let missing = PathBuf::from(format!("/tmp/bitty-cr-rich-02-missing-{pid}"));
        // Ensure it does not exist so canonicalize fails fail-closed.
        let _ = std::fs::remove_dir_all(&missing);
        let _ = std::fs::remove_file(&missing);
        assert!(!missing.exists(), "missing-root fixture must not exist");
        let err = ResourcePolicy::new(vec![missing]).unwrap_err();
        assert!(
            matches!(err, ResourceError::Io { .. }),
            "un-canonicalizable root must be rejected, got {err:?}"
        );
    }

    #[test]
    fn cr_rich_02_forbidden_root_rejected() {
        for raw in ["/proc", "/sys", "/dev"] {
            let err = ResourcePolicy::new(vec![PathBuf::from(raw)]).unwrap_err();
            assert!(
                matches!(err, ResourceError::ForbiddenPrefix { .. }),
                "forbidden root {raw} must be rejected, got {err:?}"
            );
        }
    }

    #[test]
    fn cr_rich_02_valid_absolute_root_still_works() {
        let approved = temp_root("cr-rich-02-valid");
        let outside = temp_root("cr-rich-02-valid-outside");
        let _ = std::fs::create_dir_all(&approved);
        let _ = std::fs::create_dir_all(&outside);
        let policy = ResourcePolicy::new(vec![approved.clone()])
            .expect("valid absolute root must be accepted");
        // Stored root is canonical absolute.
        assert_eq!(policy.roots().len(), 1);
        assert!(policy.roots()[0].is_absolute());
        let inside_file = write_regular_file(&approved, "inside.txt", b"inside");
        let outside_file = write_regular_file(&outside, "outside.txt", b"outside");
        let ok = validate_resource_path(&inside_file, &policy)
            .expect("inside valid root must be allowed");
        assert!(ok.starts_with(policy.roots()[0].clone()));
        assert!(policy.is_allowed(&ok));
        let canonical_outside = outside_file.canonicalize().unwrap_or(outside_file.clone());
        assert!(!policy.is_allowed(&canonical_outside));
        let err = validate_resource_path(&outside_file, &policy).unwrap_err();
        assert!(
            matches!(err, ResourceError::OutsideApprovedRoot { .. }),
            "outside valid root must be denied, got {err:?}"
        );
    }

    #[test]
    fn cr_rich_02_is_allowed_deny_by_default() {
        let deny_all = ResourcePolicy::deny_all();
        assert!(!deny_all.is_allowed(Path::new("/etc/passwd")));
        assert!(!deny_all.is_allowed(Path::new("")));
        assert!(!deny_all.is_allowed(Path::new("relative/path")));
        let empty_vec = ResourcePolicy::new(Vec::new()).unwrap();
        assert!(!empty_vec.is_allowed(Path::new("/etc/passwd")));
        // Invalid canonical inputs never allow.
        let approved = temp_root("cr-rich-02-deny-default");
        let _ = std::fs::create_dir_all(&approved);
        let policy = ResourcePolicy::new(vec![approved]).unwrap();
        assert!(!policy.is_allowed(Path::new("")));
        assert!(!policy.is_allowed(Path::new("relative/path")));
    }
}
