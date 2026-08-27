//! Lockfile types — draft per package-lifecycle RFC.
//!
//! The lockfile binds three distinct digests so that transport corruption,
//! semantic reinterpretation, and partial content tampering are independently
//! detectable (H-A, H-B, H-C).

use std::collections::BTreeMap;

use crate::error::PackageError;
use crate::integrity::validate_hex_digest;
use crate::manifest::PackageId;
use crate::source::PackageSource;

// ── digest bundle ────────────────────────────────────────────────────────

/// Triple-digest bundle for a locked package.
///
/// - `artifact` — H-A whole-artifact digest over fetched bytes.
/// - `manifest` — H-B canonical-form manifest digest over semantics.
/// - `content_root` — H-C per-file Merkle root, optional until the
///   content-addressed store lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageDigests {
    /// H-A: artifact SHA-256 hex.
    pub artifact: String,
    /// H-B: canonical manifest SHA-256 hex.
    pub manifest: String,
    /// H-C: Merkle root hex, if content-addressed store is active.
    pub content_root: Option<String>,
}

impl PackageDigests {
    /// Validate all digests are well-formed hex.
    pub fn validate(&self) -> Result<(), PackageError> {
        validate_hex_digest(&self.artifact, "digests.artifact")?;
        validate_hex_digest(&self.manifest, "digests.manifest")?;
        if let Some(r) = &self.content_root {
            validate_hex_digest(r, "digests.content_root")?;
        }
        Ok(())
    }
}

// ── locked package ───────────────────────────────────────────────────────

/// A single locked package entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedPackage {
    /// Package id.
    pub id: PackageId,
    /// Resolved version.
    pub version: String,
    /// Source that produced this resolution.
    pub source: PackageSource,
    /// Triple digest.
    pub digests: PackageDigests,
    /// When the lock was recorded (monotonic host millis, opaque for stub).
    pub locked_at: u64,
}

impl LockedPackage {
    /// Validate this entry (id, version semver, digests, source).
    pub fn validate(&self) -> Result<(), PackageError> {
        // id already validated.
        if self.version.is_empty() || self.version.len() > 64 {
            return Err(PackageError::lockfile(format!(
                "locked package {} version invalid: '{}'",
                self.id, self.version
            )));
        }
        // Basic semver sanity (reuse manifest's validate_semver logic via re-check).
        // For draft, just check contains dot and valid chars.
        if !self.version.contains('.') {
            return Err(PackageError::lockfile(format!(
                "locked package {} version '{}' malformed",
                self.id, self.version
            )));
        }
        self.digests.validate()?;
        self.source.validate()?;
        Ok(())
    }

    /// Whether artifact digest matches `bytes`.
    #[must_use]
    pub fn artifact_matches(&self, bytes: &[u8]) -> bool {
        crate::integrity::sha256_hex(bytes).eq_ignore_ascii_case(&self.digests.artifact)
    }
}

// ── lockfile ─────────────────────────────────────────────────────────────

/// Current lockfile format version.
pub const LOCKFILE_VERSION: u32 = 1;

/// Owned lockfile — full resolution snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lockfile {
    /// Format version.
    pub version: u32,
    /// Locked packages, keyed insertion order.
    pub packages: Vec<LockedPackage>,
    /// Canonical encoding version for H-B (frozen per format).
    pub canonical_version: String,
}

impl Lockfile {
    /// Create a new empty lockfile at current version.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: LOCKFILE_VERSION,
            packages: Vec::new(),
            canonical_version: "bitty-manifest-v1".to_string(),
        }
    }

    /// Validate the lockfile (version, duplicates, per-entry validation).
    pub fn validate(&self) -> Result<(), PackageError> {
        if self.version != LOCKFILE_VERSION {
            return Err(PackageError::lockfile(format!(
                "unsupported lockfile version {}, expected {}",
                self.version, LOCKFILE_VERSION
            )));
        }
        if self.canonical_version.trim().is_empty() {
            return Err(PackageError::lockfile(
                "canonical_version must not be empty",
            ));
        }
        if self.packages.len() > 1024 {
            return Err(PackageError::LimitExceeded {
                field: "lockfile.packages".to_string(),
                limit: 1024,
                actual: self.packages.len(),
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for p in &self.packages {
            p.validate()?;
            if !seen.insert(p.id.as_str().to_string()) {
                return Err(PackageError::Duplicate {
                    kind: "locked package".to_string(),
                    value: p.id.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Insert a locked package; errors on duplicate id.
    pub fn insert(&mut self, pkg: LockedPackage) -> Result<(), PackageError> {
        if self.packages.iter().any(|p| p.id == pkg.id) {
            return Err(PackageError::Duplicate {
                kind: "locked package".to_string(),
                value: pkg.id.to_string(),
            });
        }
        pkg.validate()?;
        self.packages.push(pkg);
        Ok(())
    }

    /// Find a locked package by id.
    #[must_use]
    pub fn get(&self, id: &PackageId) -> Option<&LockedPackage> {
        self.packages.iter().find(|p| &p.id == id)
    }

    /// Remove a locked package by id.
    pub fn remove(&mut self, id: &PackageId) -> Result<LockedPackage, PackageError> {
        let pos = self
            .packages
            .iter()
            .position(|p| &p.id == id)
            .ok_or_else(|| PackageError::NotFound { id: id.to_string() })?;
        Ok(self.packages.remove(pos))
    }

    /// Deterministic digest over the lockfile for generation binding.
    ///
    /// Sorts packages by id and hashes concatenated digests + versions.
    #[must_use]
    pub fn digest(&self) -> String {
        let mut sorted: BTreeMap<&str, &LockedPackage> = BTreeMap::new();
        for p in &self.packages {
            sorted.insert(p.id.as_str(), p);
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"bitty-lock-v1\n");
        for (_, p) in sorted {
            bytes.extend_from_slice(p.id.as_str().as_bytes());
            bytes.push(b'@');
            bytes.extend_from_slice(p.version.as_bytes());
            bytes.push(b':');
            bytes.extend_from_slice(p.digests.artifact.as_bytes());
            bytes.push(b':');
            bytes.extend_from_slice(p.digests.manifest.as_bytes());
            if let Some(r) = &p.digests.content_root {
                bytes.push(b':');
                bytes.extend_from_slice(r.as_bytes());
            }
            bytes.push(b'\n');
        }
        crate::integrity::sha256_hex(&bytes)
    }

    /// Number of packages.
    #[must_use]
    pub fn len(&self) -> usize {
        self.packages.len()
    }

    /// True when no packages are locked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.packages.is_empty()
    }
}

impl Default for Lockfile {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrity::{is_valid_hex_digest, sha256_hex};
    use crate::manifest::PackageId;
    use crate::source::PackageSource;

    fn test_digests() -> PackageDigests {
        PackageDigests {
            artifact: sha256_hex(b"artifact"),
            manifest: sha256_hex(b"manifest"),
            content_root: None,
        }
    }

    fn locked_pkg(id: &str) -> LockedPackage {
        LockedPackage {
            id: PackageId::new(id).unwrap(),
            version: "0.1.0".to_string(),
            source: PackageSource::Registry {
                url: "https://example.com/registry".to_string(),
            },
            digests: test_digests(),
            locked_at: 1,
        }
    }

    #[test]
    fn lockfile_round_trip() {
        let mut lf = Lockfile::new();
        lf.insert(locked_pkg("xuepoo.a")).unwrap();
        lf.insert(locked_pkg("xuepoo.b")).unwrap();
        lf.validate().unwrap();
        assert_eq!(lf.len(), 2);
        // Digest determinism: order shouldn't matter.
        let mut lf2 = Lockfile::new();
        lf2.insert(locked_pkg("xuepoo.b")).unwrap();
        lf2.insert(locked_pkg("xuepoo.a")).unwrap();
        assert_eq!(lf.digest(), lf2.digest());
    }

    #[test]
    fn duplicate_rejected() {
        let mut lf = Lockfile::new();
        lf.insert(locked_pkg("xuepoo.a")).unwrap();
        assert!(lf.insert(locked_pkg("xuepoo.a")).is_err());
    }

    #[test]
    fn invalid_digest_rejected() {
        let mut p = locked_pkg("xuepoo.a");
        p.digests.artifact = "not-hex".to_string();
        assert!(p.validate().is_err());
    }

    #[test]
    fn unsupported_version_rejected() {
        let mut lf = Lockfile::new();
        lf.version = 999;
        assert!(lf.validate().is_err());
    }

    #[test]
    fn with_content_root_h_c() {
        let mut p = locked_pkg("xuepoo.a");
        p.digests.content_root = Some(sha256_hex(b"root"));
        p.validate().unwrap();
        let mut lf = Lockfile::new();
        lf.insert(p).unwrap();
        lf.validate().unwrap();
    }

    #[test]
    fn tampered_digest_detected() {
        let p = locked_pkg("xuepoo.a");
        assert!(p.artifact_matches(b"artifact"));
        assert!(!p.artifact_matches(b"tampered"));
    }

    #[test]
    fn valid_hex_helper() {
        assert!(is_valid_hex_digest(&sha256_hex(b"x")));
    }
}
