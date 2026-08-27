//! Package source types and local-path trust separation.
//!
//! Every source type passes the full 7-stage integrity chain; local-path
//! packages use degenerate records rather than exemptions (PL-AC-005).

use crate::error::PackageError;
use crate::integrity::{is_valid_hex_digest, sha256_hex};

// ── limits ───────────────────────────────────────────────────────────────

/// Maximum source URL length.
pub const MAX_SOURCE_URL_LEN: usize = 2048;
/// Maximum resolved revision length (git SHA, etc.).
pub const MAX_REV_LEN: usize = 256;
/// Maximum local path length.
pub const MAX_PATH_LEN: usize = 1024;

// ── source enum ──────────────────────────────────────────────────────────

/// Source that produced a package.
///
/// The RFC requires identical verification for every variant; local-path
/// never claims registry provenance, and content changes are detected via
/// re-digestion on every sync/update.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PackageSource {
    /// Registry source (future). The URL is the registry base, not per-package.
    Registry {
        /// Registry base URL, e.g. `https://registry.bitty.dev`.
        url: String,
    },
    /// Git source.
    Git {
        /// Repository URL.
        url: String,
        /// Resolved revision (commit SHA or tag). `None` until resolved.
        rev: Option<String>,
    },
    /// Local path for development.
    LocalPath {
        /// Filesystem path (owned, as supplied).
        path: String,
        /// Content digest captured at resolution time (H-A over directory hash stub).
        content_digest: String,
    },
    /// Bundled with the host (outside generation model or not — open item).
    Bundled,
}

impl PackageSource {
    /// Human label for the source kind.
    #[must_use]
    pub fn kind_label(&self) -> &'static str {
        match self {
            Self::Registry { .. } => "registry",
            Self::Git { .. } => "git",
            Self::LocalPath { .. } => "local-path",
            Self::Bundled => "bundled",
        }
    }

    /// Whether this is a local-path source.
    #[must_use]
    pub fn is_local_path(&self) -> bool {
        matches!(self, Self::LocalPath { .. })
    }

    /// Validate this source (bounded, hex where required, no provenance confusion).
    pub fn validate(&self) -> Result<(), PackageError> {
        match self {
            Self::Registry { url } => {
                if url.trim().is_empty() {
                    return Err(PackageError::source("registry url must not be empty"));
                }
                if url.len() > MAX_SOURCE_URL_LEN {
                    return Err(PackageError::LimitExceeded {
                        field: "source.registry.url".to_string(),
                        limit: MAX_SOURCE_URL_LEN,
                        actual: url.len(),
                    });
                }
                if url.contains(' ') {
                    return Err(PackageError::source("registry url must not contain spaces"));
                }
            }
            Self::Git { url, rev } => {
                if url.trim().is_empty() {
                    return Err(PackageError::source("git url must not be empty"));
                }
                if url.len() > MAX_SOURCE_URL_LEN {
                    return Err(PackageError::LimitExceeded {
                        field: "source.git.url".to_string(),
                        limit: MAX_SOURCE_URL_LEN,
                        actual: url.len(),
                    });
                }
                if let Some(r) = rev {
                    if r.len() > MAX_REV_LEN {
                        return Err(PackageError::LimitExceeded {
                            field: "source.git.rev".to_string(),
                            limit: MAX_REV_LEN,
                            actual: r.len(),
                        });
                    }
                    if r.trim().is_empty() {
                        return Err(PackageError::source(
                            "git rev when present must not be empty",
                        ));
                    }
                }
            }
            Self::LocalPath {
                path,
                content_digest,
            } => {
                if path.trim().is_empty() {
                    return Err(PackageError::source("local-path path must not be empty"));
                }
                if path.len() > MAX_PATH_LEN {
                    return Err(PackageError::LimitExceeded {
                        field: "source.local_path.path".to_string(),
                        limit: MAX_PATH_LEN,
                        actual: path.len(),
                    });
                }
                if path.contains('\0') {
                    return Err(PackageError::source("local-path path must not contain NUL"));
                }
                if !is_valid_hex_digest(content_digest) {
                    return Err(PackageError::source(format!(
                        "local-path content_digest '{content_digest}' is not valid 64-hex"
                    )));
                }
            }
            Self::Bundled => {}
        }
        Ok(())
    }

    /// For non-local-path sources, the registry-class provenance flag is true.
    ///
    /// Local-path never has this flag; it cannot be republished or promoted
    /// without passing the full chain as its own artifact.
    #[must_use]
    pub fn has_registry_provenance(&self) -> bool {
        matches!(self, Self::Registry { .. })
    }
}

// ── local-path drift helpers ─────────────────────────────────────────────

/// Compute a content digest for local-path packages.
///
/// In production this hashes the directory tree; for this draft it hashes
/// the concatenated file bytes supplied by the caller (pure, headless).
/// Returns 64-hex SHA-256.
#[must_use]
pub fn digest_local_content(files: &[(&str, &[u8])]) -> String {
    // Deterministic: sort by path, then hash each file's bytes with path prefix.
    let mut sorted: Vec<(&str, &[u8])> = files.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut all = Vec::new();
    for (path, bytes) in sorted {
        all.extend_from_slice(path.as_bytes());
        all.push(b'\0');
        all.extend_from_slice(bytes);
        all.push(b'\n');
    }
    sha256_hex(&all)
}

/// Check whether local content has drifted since the lock was recorded.
///
/// Returns `Ok(())` when digests match, `Err(LocalPathDrift)` when they differ.
pub fn check_local_path_drift(
    package_id: &str,
    recorded_digest: &str,
    current_files: &[(&str, &[u8])],
) -> Result<(), PackageError> {
    let current = digest_local_content(current_files);
    if !current.eq_ignore_ascii_case(recorded_digest) {
        return Err(PackageError::LocalPathDrift {
            package: package_id.to_string(),
            recorded: recorded_digest.to_ascii_lowercase(),
            current,
        });
    }
    Ok(())
}

/// Validate that a local-path package is not being promoted to registry provenance.
///
/// Callers that construct a new lock entry from a local-path source must not
/// set registry provenance; this helper fails closed if they attempt it.
pub fn ensure_no_promotion_without_chain(
    source: &PackageSource,
    claims_registry: bool,
) -> Result<(), PackageError> {
    if source.is_local_path() && claims_registry {
        return Err(PackageError::source(
            "local-path package cannot claim registry provenance without passing the full verification chain as its own artifact",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_validation() {
        let s = PackageSource::Registry {
            url: "https://registry.example.com".to_string(),
        };
        s.validate().unwrap();
        let bad = PackageSource::Registry {
            url: "".to_string(),
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn local_path_digest_determinism() {
        let files_a = vec![("a.txt", b"hello" as &[u8]), ("b.txt", b"world" as &[u8])];
        let files_b = vec![("b.txt", b"world" as &[u8]), ("a.txt", b"hello" as &[u8])];
        assert_eq!(
            digest_local_content(&files_a),
            digest_local_content(&files_b)
        );
    }

    #[test]
    fn drift_detection() {
        let files = vec![("a.txt", b"hello" as &[u8])];
        let d = digest_local_content(&files);
        let source = PackageSource::LocalPath {
            path: "/tmp/pkg".to_string(),
            content_digest: d.clone(),
        };
        source.validate().unwrap();
        // No drift.
        check_local_path_drift("xuepoo.pkg", &d, &files).unwrap();
        // Drift: file changed.
        let changed = vec![("a.txt", b"changed" as &[u8])];
        assert!(check_local_path_drift("xuepoo.pkg", &d, &changed).is_err());
        // Drift: file added.
        let added = vec![("a.txt", b"hello" as &[u8]), ("b.txt", b"new" as &[u8])];
        assert!(check_local_path_drift("xuepoo.pkg", &d, &added).is_err());
        // Drift: file removed.
        let removed: Vec<(&str, &[u8])> = vec![];
        assert!(check_local_path_drift("xuepoo.pkg", &d, &removed).is_err());
    }

    #[test]
    fn promotion_blocked() {
        let s = PackageSource::LocalPath {
            path: "/tmp/pkg".to_string(),
            content_digest: "a".repeat(64),
        };
        assert!(ensure_no_promotion_without_chain(&s, true).is_err());
        assert!(ensure_no_promotion_without_chain(&s, false).is_ok());
        let reg = PackageSource::Registry {
            url: "https://example.com".to_string(),
        };
        assert!(ensure_no_promotion_without_chain(&reg, true).is_ok());
    }

    #[test]
    fn local_path_provenance() {
        let s = PackageSource::LocalPath {
            path: "/tmp".to_string(),
            content_digest: "b".repeat(64),
        };
        assert!(!s.has_registry_provenance());
        let r = PackageSource::Registry {
            url: "https://example.com".to_string(),
        };
        assert!(r.has_registry_provenance());
    }

    #[test]
    fn local_path_no_registry_claim_on_validate() {
        // Direct validation of a local source never claims registry provenance internally.
        let s = PackageSource::LocalPath {
            path: "./my-pkg".to_string(),
            content_digest: "c".repeat(64),
        };
        s.validate().unwrap();
    }
}
