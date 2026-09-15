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
/// Maximum host length (DNS 253).
const MAX_HOST_LEN: usize = 253;
/// Maximum userinfo length for `git+ssh` URLs.
const MAX_USERINFO_LEN: usize = 64;

// ── URL scheme policy (CTX-0466) ─────────────────────────────────────────
//
// Registry fetches artifacts and must use TLS: `https://` only.
// Git may use `https://` (TLS) or `git+ssh://` (explicit SSH transport).
// Everything else — `http`, `ftp`, `file`, `ssh://`, `git://`, scp-like
// `git@host:path`, `javascript:`, `data:`, schemeless paths — is rejected
// fail-closed in v1. Plain `ssh://` and scp-like syntax are deferred to a
// future RFC; use the explicit `git+ssh://` form.

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
                validate_source_url(url, &["https://"], "source.registry.url")?;
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
                validate_source_url(url, &["https://", "git+ssh://"], "source.git.url")?;
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
                    validate_git_rev(r)?;
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

// ── URL + rev validation (CTX-0466, hand-rolled, no new deps) ───────────

/// Validate a registry/git URL against an allowlisted scheme set.
///
/// Checks, fail-closed: no control/space/backslash anywhere, exact lowercase
/// scheme prefix, non-empty authority, valid host (charset + label rules),
/// optional numeric port, and a remainder with no control/space/backslash.
/// Userinfo (`user@`) is rejected for `https://` and allowed only for
/// `git+ssh://` with a restricted charset (the `git@` convention).
fn validate_source_url(url: &str, allowed: &[&str], field: &str) -> Result<(), PackageError> {
    if url
        .bytes()
        .any(|b| b.is_ascii_control() || b == b' ' || b == b'\\')
    {
        return Err(PackageError::source(format!(
            "{field} must not contain whitespace, control characters, or backslashes"
        )));
    }
    let scheme = allowed
        .iter()
        .find(|s| url.starts_with(**s))
        .ok_or_else(|| {
            PackageError::source(format!(
                "{field} scheme must be one of {} (got '{url}')",
                allowed.join(", ")
            ))
        })?;
    let rest = &url[scheme.len()..];
    if rest.is_empty() {
        return Err(PackageError::source(format!("{field} is missing a host")));
    }
    // Authority ends at the first '/', '?', or '#'.
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    let remainder = &rest[auth_end..];
    if authority.is_empty() {
        return Err(PackageError::source(format!("{field} is missing a host")));
    }
    // Userinfo handling: only `git+ssh://` may carry `user@`.
    let allow_userinfo = *scheme == "git+ssh://";
    let hostport = if let Some(at) = authority.rfind('@') {
        if !allow_userinfo {
            return Err(PackageError::source(format!(
                "{field} must not contain userinfo"
            )));
        }
        let userinfo = &authority[..at];
        let hostport = &authority[at + 1..];
        if userinfo.is_empty()
            || userinfo.len() > MAX_USERINFO_LEN
            || !userinfo
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        {
            return Err(PackageError::source(format!(
                "{field} has invalid userinfo"
            )));
        }
        if hostport.is_empty() {
            return Err(PackageError::source(format!("{field} is missing a host")));
        }
        hostport
    } else {
        authority
    };
    // Optional port.
    let host = if let Some(colon) = hostport.rfind(':') {
        let (h, port_str) = (&hostport[..colon], &hostport[colon + 1..]);
        if h.is_empty() {
            return Err(PackageError::source(format!("{field} is missing a host")));
        }
        if port_str.is_empty()
            || port_str.len() > 5
            || !port_str.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(PackageError::source(format!("{field} has invalid port")));
        }
        let port: u32 = port_str
            .parse()
            .map_err(|_| PackageError::source(format!("{field} has invalid port")))?;
        if port == 0 || port > 65535 {
            return Err(PackageError::source(format!("{field} has invalid port")));
        }
        h
    } else {
        hostport
    };
    validate_host(host, field)?;
    // Remainder (path/query/fragment): no control/space/backslash (already
    // checked globally; re-assert for a precise message).
    if remainder
        .bytes()
        .any(|b| b.is_ascii_control() || b == b' ' || b == b'\\')
    {
        return Err(PackageError::source(format!(
            "{field} path must not contain whitespace, control characters, or backslashes"
        )));
    }
    Ok(())
}

/// Validate a DNS-style host: charset, length, and per-label rules.
fn validate_host(host: &str, field: &str) -> Result<(), PackageError> {
    if host.is_empty() || host.len() > MAX_HOST_LEN {
        return Err(PackageError::source(format!("{field} has invalid host")));
    }
    if !host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    {
        return Err(PackageError::source(format!("{field} has invalid host")));
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(PackageError::source(format!("{field} has invalid host")));
        }
        let bytes = label.as_bytes();
        if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
            return Err(PackageError::source(format!("{field} has invalid host")));
        }
        if !bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
        {
            return Err(PackageError::source(format!("{field} has invalid host")));
        }
    }
    Ok(())
}

/// Validate a git revision: closed charset plus structural guards.
///
/// Allowed: ASCII alphanumeric plus `. - _ / +`. Additionally the rev must
/// start and end alphanumeric (rejects leading `-//.` option/path confusion
/// and trailing slashes), and must not contain `..` (range/parent operator)
/// or `//` (empty segment). Length is enforced by the caller (`MAX_REV_LEN`
/// kept); this function enforces charset and structure. `None` (unresolved)
/// is handled by the caller.
fn validate_git_rev(rev: &str) -> Result<(), PackageError> {
    if !rev
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'/' | b'+'))
    {
        return Err(PackageError::source(
            "git rev contains invalid characters (allowed: A-Z a-z 0-9 . - _ / +)",
        ));
    }
    let bytes = rev.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return Err(PackageError::source(
            "git rev must start and end with an alphanumeric character",
        ));
    }
    if rev.contains("..") || rev.contains("//") {
        return Err(PackageError::source(
            "git rev must not contain '..' or '//'",
        ));
    }
    Ok(())
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

    #[test]
    fn registry_accepts_legit_https() {
        for url in [
            "https://registry.example.com",
            "https://example.com",
            "https://example.com/registry",
            "https://registry.example.com:443/v1",
            "https://localhost:8080/registry",
        ] {
            let s = PackageSource::Registry {
                url: url.to_string(),
            };
            assert!(s.validate().is_ok(), "legit registry '{url}' must pass");
        }
    }

    #[test]
    fn registry_rejects_schemeless_and_malicious() {
        // CTX-0466: schemeless / non-https / missing-host URLs must fail closed.
        for url in [
            "registry.example.com",
            "example.com/foo",
            "/etc/passwd",
            "http://example.com",
            "ftp://example.com/pkg",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/plain,hi",
            "git://example.com/repo.git",
            "ssh://example.com/repo.git",
            "https://",
            "https:///path",
            "https://exa mple.com",
            "https://example..com",
            "https://-bad.com",
            "https://bad-.com",
            "https://user:pass@example.com",
            "https://example.com/foo bar",
            "HTTPS://example.com",
        ] {
            let s = PackageSource::Registry {
                url: url.to_string(),
            };
            assert!(s.validate().is_err(), "registry '{url}' must be rejected");
        }
    }

    #[test]
    fn git_accepts_https_and_git_ssh() {
        for url in [
            "https://github.com/owner/repo.git",
            "https://example.com/owner/repo",
            "git+ssh://github.com/owner/repo.git",
            "git+ssh://git@github.com/owner/repo.git",
        ] {
            let s = PackageSource::Git {
                url: url.to_string(),
                rev: None,
            };
            assert!(s.validate().is_ok(), "legit git '{url}' must pass");
        }
    }

    #[test]
    fn git_rejects_schemeless_and_malicious() {
        // scp-like syntax, plain ssh/git/file/http, and malformed hosts fail closed.
        for url in [
            "github.com/owner/repo.git",
            "git@github.com:owner/repo.git",
            "ssh://github.com/owner/repo.git",
            "git://github.com/owner/repo.git",
            "http://github.com/owner/repo.git",
            "file:///tmp/repo",
            "ftp://example.com/repo.git",
            "javascript:alert(1)",
            "https://",
            "git+ssh://",
            "https://exa mple.com/repo.git",
            "https://example..com/repo.git",
            "git+ssh://example..com/repo.git",
            "https://user:pass@example.com/repo.git",
            "https://example.com/repo bar.git",
            "HTTPS://example.com/repo.git",
        ] {
            let s = PackageSource::Git {
                url: url.to_string(),
                rev: None,
            };
            assert!(s.validate().is_err(), "git '{url}' must be rejected");
        }
    }

    #[test]
    fn git_rev_accepts_legit_and_rejects_hostile() {
        // Legit: full SHAs, tags, branches (None means unresolved and is ok).
        let ok_revs = [
            "abc123def456789012345678901234567890abcd",
            "v1.2.3",
            "main",
            "feature/foo",
            "refs/tags/v1.0.0",
        ];
        for rev in ok_revs {
            let s = PackageSource::Git {
                url: "https://github.com/owner/repo.git".to_string(),
                rev: Some(rev.to_string()),
            };
            assert!(s.validate().is_ok(), "legit rev '{rev}' must pass");
        }
        let bad_revs = [
            "HEAD; rm -rf /",
            "$(evil)",
            "`evil`",
            "rev with space",
            "rev\nnewline",
            "rev\0nul",
            "--upload-pack=evil",
            "../escape",
            "a..b",
            "a//b",
            "/leading",
            "trailing/",
            "-leading-dash",
            ".leading-dot",
            "",
            "   ",
        ];
        for rev in bad_revs {
            let s = PackageSource::Git {
                url: "https://github.com/owner/repo.git".to_string(),
                rev: Some(rev.to_string()),
            };
            assert!(s.validate().is_err(), "rev '{rev}' must be rejected");
        }
        // Oversized rev rejected, 256B cap kept.
        let oversized = "a".repeat(MAX_REV_LEN + 1);
        let s = PackageSource::Git {
            url: "https://github.com/owner/repo.git".to_string(),
            rev: Some(oversized),
        };
        assert!(s.validate().is_err());
        // Exactly at cap with valid charset passes.
        let at_cap = "a".repeat(MAX_REV_LEN);
        let s2 = PackageSource::Git {
            url: "https://github.com/owner/repo.git".to_string(),
            rev: Some(at_cap),
        };
        assert!(s2.validate().is_ok());
    }
}
