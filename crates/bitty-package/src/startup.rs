//! Core read-only startup validation of an already-installed generation.
//!
//! This is the `W-101` / `CTX-0927` first slice: an explicit, pure entry point
//! for the read-only half of the package boundary. It re-derives the verdict
//! Core needs before the plugin host instantiates a VM, using only the
//! retained primitives in this crate — never fetching, resolving, installing,
//! activating, or mutating the store.
//!
//! # What Core re-verifies
//!
//! 1. **Parse/validate.** The installed manifest was parsed with the bounded
//!    parser; [`PackageManifest::validate`] re-checks the schema, limits, and
//!    the closed capability set (`P0-AC-027`).
//! 2. **Artifact integrity (`H-A`).** The staged bytes are re-digested and
//!    compared with the lock record.
//! 3. **Manifest binding (`H-B`).** The canonical manifest digest is
//!    re-derived and compared with the lock record (`P0-AC-028`).
//! 4. **Grant snapshot.** Every recorded grant must parse as a capability in
//!    the closed set and must be declared by the manifest; an over-broad or
//!    underivable grant fails closed and the plugin is not loaded
//!    (`P0-AC-012`, deny-by-default).
//!
//! Compatibility and content-root checks stay with the runtime load path that
//! owns the staged module tree; this function is the pure integrity and
//! capability half.
//!
//! # Core-never-network
//!
//! No function here opens a socket, resolves a name, or reads remote
//! metadata. The only inputs are already-installed bytes and recorded data.
//! This is the `DIR-016`/`DIR-017` invariant: Core never fetches.
//!
//! # Wiring status (W-101 CTX-0944)
//!
//! The runtime load path (`bitty-runtime` `resolve_record`) digests the staged
//! Lua module tree rather than a single artifact blob. The tree digest is
//! reconciled with the `H-A` artifact digest by sharing one scheme: the
//! canonical tree buffer owned by [`crate::source::canonical_tree_bytes`].
//! Staged trees validate through [`validate_staged_tree_generation`], which
//! reuses the same `H-A` primitive ([`verify_artifact_checksum`]) and the same
//! grant-snapshot rule as [`validate_installed_generation`], but binds the
//! runtime manifest's canonical bytes generically instead of requiring a
//! `PackageManifest`. The blob entry point below stays untouched for the
//! package-manager generation model; see [`crate::boundary`] for the audit.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;

use crate::error::PackageError;
use crate::integrity::{
    is_valid_hex_digest, sha256_hex, verify_artifact_checksum, verify_manifest,
    verify_manifest_hash_binding,
};
use crate::manifest::{CapabilityId, PackageManifest};

/// Label used for grant-snapshot failures in diagnostics.
pub const STARTUP_GRANT_SNAPSHOT_STAGE: &str = "startup_grant_snapshot";

/// Inputs for the read-only startup re-verification of one installed
/// generation (pure data, no I/O).
#[derive(Debug, Clone)]
pub struct InstalledGenerationInputs<'a> {
    /// Already-parsed installed manifest (bounded parser output).
    pub manifest: &'a PackageManifest,
    /// Already-installed artifact bytes read read-only from the store.
    pub artifact_bytes: &'a [u8],
    /// Expected artifact digest `H-A` from the lock record (64 hex).
    pub expected_artifact_digest: &'a str,
    /// Expected canonical manifest digest `H-B` from the lock record (64 hex).
    pub expected_manifest_digest: &'a str,
    /// Recorded capability grant snapshot for this exact manifest hash.
    pub granted_capabilities: &'a [String],
}

/// Re-derive Core's read-only verdict over an installed generation.
///
/// Fails closed on the first mismatch: a tampered artifact or manifest, an
/// invalid manifest, or a grant snapshot that is not a subset of the
/// manifest's closed capability set all return an owned [`PackageError`] and
/// the caller must not load the plugin. No network access and no store
/// mutation occurs.
///
/// # Errors
///
/// [`PackageError::Integrity`], [`PackageError::DigestMismatch`], or
/// [`PackageError::ManifestHashMismatch`] as produced by the retained
/// integrity primitives, plus [`PackageError::Integrity`] with stage
/// [`STARTUP_GRANT_SNAPSHOT_STAGE`] for a grant that cannot be re-derived.
pub fn validate_installed_generation(
    inputs: &InstalledGenerationInputs<'_>,
) -> Result<(), PackageError> {
    // Step 2 (parse/validate): bounded schema, limits, closed capabilities.
    verify_manifest(inputs.manifest)?;
    // Step 3 (H-A): staged bytes match the lock artifact digest.
    verify_artifact_checksum(inputs.artifact_bytes, inputs.expected_artifact_digest)?;
    // Step 4 (H-B): canonical manifest digest matches the lock record.
    verify_manifest_hash_binding(inputs.manifest, inputs.expected_manifest_digest)?;
    // Step 5: recorded grant snapshot re-derivation, deny-by-default.
    validate_grant_snapshot(inputs.manifest, inputs.granted_capabilities)?;
    Ok(())
}

/// Re-derive the recorded grant snapshot against the manifest's closed set.
///
/// Every recorded grant must be a valid closed-set capability and must be
/// declared by the manifest; a grant that a plugin did not declare (or that
/// the closed set does not contain) fails closed. A narrowed grant is
/// accepted: missing authority denies at call time, never widens here.
fn validate_grant_snapshot(
    manifest: &PackageManifest,
    granted_capabilities: &[String],
) -> Result<(), PackageError> {
    let declared: Vec<String> = manifest
        .capabilities
        .iter()
        .map(|capability| capability.as_str().to_string())
        .collect();
    validate_grant_snapshot_strings(&declared, granted_capabilities)
}

/// Re-derive a recorded grant snapshot against declared capability strings.
///
/// Shared by [`validate_installed_generation`] (via [`validate_grant_snapshot`])
/// and [`validate_staged_tree_generation`]: the closed-set parse plus
/// declared-subset rule live in one place so the blob and tree paths cannot
/// drift. Pure, read-only, no I/O.
fn validate_grant_snapshot_strings(
    declared_capabilities: &[String],
    granted_capabilities: &[String],
) -> Result<(), PackageError> {
    let declared: BTreeSet<&str> = declared_capabilities
        .iter()
        .map(|entry| entry.as_str())
        .collect();
    for raw in granted_capabilities {
        let capability = CapabilityId::new(raw).map_err(|error| {
            PackageError::integrity(
                STARTUP_GRANT_SNAPSHOT_STAGE,
                format!("recorded grant '{raw}' is not a closed capability: {error}"),
            )
        })?;
        if !declared.contains(capability.as_str()) {
            return Err(PackageError::integrity(
                STARTUP_GRANT_SNAPSHOT_STAGE,
                format!(
                    "recorded grant '{}' is not declared by the installed manifest",
                    capability.as_str()
                ),
            ));
        }
    }
    Ok(())
}

/// Verify a generic manifest binding (`H-B`) over caller-supplied canonical bytes.
///
/// Same fail-closed shape as [`verify_manifest_hash_binding`] but without
/// requiring a `PackageManifest`: the runtime passes its own
/// `canonical_bytes` (e.g. the Lua plugin manifest) and the record's expected
/// digest. Pure, read-only.
fn verify_manifest_bytes_binding(
    manifest_canonical_bytes: &[u8],
    expected_hex: &str,
) -> Result<(), PackageError> {
    if !is_valid_hex_digest(expected_hex) {
        return Err(PackageError::integrity(
            "manifest_hash_binding",
            format!("expected digest '{expected_hex}' is not valid 64-hex"),
        ));
    }
    let actual = sha256_hex(manifest_canonical_bytes);
    if !actual.eq_ignore_ascii_case(expected_hex) {
        return Err(PackageError::ManifestHashMismatch {
            expected: expected_hex.to_ascii_lowercase(),
            actual,
        });
    }
    Ok(())
}

/// Inputs for the read-only startup re-verification of one staged Lua module
/// tree (pure data, no I/O).
///
/// Unlike [`InstalledGenerationInputs`] (a single artifact blob bound to a
/// `PackageManifest`), this binds the canonical tree buffer from
/// [`crate::source::canonical_tree_bytes`] as `H-A` and the caller's manifest
/// canonical bytes as `H-B`. The runtime load path supplies the staged tree
/// it already walked read-only; no fetch, resolve, activate, trust, or store
/// mutation occurs.
#[derive(Debug, Clone)]
pub struct StagedTreeInputs<'a> {
    /// Canonical tree buffer (length-delimited `len || path || len || content`, sorted).
    pub tree_bytes: &'a [u8],
    /// Expected tree digest `H-A` from the record (64 hex).
    pub expected_tree_digest: &'a str,
    /// Canonical manifest bytes the record's `H-B` was derived from.
    pub manifest_canonical_bytes: &'a [u8],
    /// Expected canonical manifest digest `H-B` from the record (64 hex).
    pub expected_manifest_digest: &'a str,
    /// Declared capabilities from the bound manifest (strings).
    pub declared_capabilities: &'a [String],
    /// Recorded capability grant snapshot for this exact manifest hash.
    pub granted_capabilities: &'a [String],
}

/// Re-derive Core's read-only verdict over a staged module tree.
///
/// Fails closed on the first mismatch: a tampered tree or manifest binding,
/// or a grant snapshot that is not a subset of the declared closed set, all
/// return an owned [`PackageError`] and the caller must not load the plugin.
/// Shares the `H-A` primitive and the grant rule with
/// [`validate_installed_generation`]; `H-B` is the same SHA-256 comparison
/// over caller-supplied canonical bytes. No network access and no store
/// mutation occurs.
///
/// # Errors
///
/// [`PackageError::Integrity`], [`PackageError::DigestMismatch`], or
/// [`PackageError::ManifestHashMismatch`] for integrity/binding mismatches,
/// plus [`PackageError::Integrity`] with stage
/// [`STARTUP_GRANT_SNAPSHOT_STAGE`] for a grant that cannot be re-derived.
pub fn validate_staged_tree_generation(inputs: &StagedTreeInputs<'_>) -> Result<(), PackageError> {
    // H-A: the staged tree buffer matches the record's content digest. The
    // buffer scheme is owned by `crate::source::canonical_tree_bytes`, so the
    // install-time and runtime digests cannot drift.
    verify_artifact_checksum(inputs.tree_bytes, inputs.expected_tree_digest)?;
    // H-B: the caller's canonical manifest bytes match the record.
    verify_manifest_bytes_binding(
        inputs.manifest_canonical_bytes,
        inputs.expected_manifest_digest,
    )?;
    // Grant snapshot re-derivation, deny-by-default.
    validate_grant_snapshot_strings(inputs.declared_capabilities, inputs.granted_capabilities)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrity::sha256_hex;
    use crate::manifest::{Compat, PackageId, PackageIdentity};

    const ARTIFACT: &[u8] = b"installed plugin artifact bytes";

    fn manifest() -> PackageManifest {
        PackageManifest {
            identity: PackageIdentity {
                id: PackageId::new("xuepoo.installed").expect("valid package id"),
                name: "Installed".to_string(),
                version: "0.1.0".to_string(),
                description: "installed fixture".to_string(),
                license: Some("MIT".to_string()),
            },
            compat: Compat {
                bitty: Some(">=0.5.0,<1.0.0".to_string()),
                plugin_api: Some("^1.0".to_string()),
            },
            dependencies: Vec::new(),
            capabilities: vec![CapabilityId::new("terminal.semantic-read").unwrap()],
            raw_bytes_len: 256,
            undeclared_fields: Vec::new(),
        }
    }

    /// Owns the digest strings so `InstalledGenerationInputs` can borrow them.
    struct Fixture {
        manifest: PackageManifest,
        artifact_digest: String,
        manifest_digest: String,
    }

    impl Fixture {
        fn new(manifest: PackageManifest) -> Self {
            let artifact_digest = sha256_hex(ARTIFACT);
            let manifest_digest = manifest.canonical_digest();
            Self {
                manifest,
                artifact_digest,
                manifest_digest,
            }
        }

        fn inputs<'a>(
            &'a self,
            artifact_bytes: &'a [u8],
            granted: &'a [String],
        ) -> InstalledGenerationInputs<'a> {
            InstalledGenerationInputs {
                manifest: &self.manifest,
                artifact_bytes,
                expected_artifact_digest: &self.artifact_digest,
                expected_manifest_digest: &self.manifest_digest,
                granted_capabilities: granted,
            }
        }
    }

    #[test]
    fn valid_installed_generation_passes() {
        let fixture = Fixture::new(manifest());
        let granted = vec!["terminal.semantic-read".to_string()];
        assert!(validate_installed_generation(&fixture.inputs(ARTIFACT, &granted)).is_ok());
    }

    #[test]
    fn tampered_artifact_fails_closed() {
        let fixture = Fixture::new(manifest());
        let granted = vec!["terminal.semantic-read".to_string()];
        assert!(
            validate_installed_generation(&fixture.inputs(b"tampered bytes", &granted)).is_err()
        );
    }

    #[test]
    fn tampered_manifest_digest_fails_closed() {
        let fixture = Fixture::new(manifest());
        let granted = vec!["terminal.semantic-read".to_string()];
        let inputs = InstalledGenerationInputs {
            manifest: &fixture.manifest,
            artifact_bytes: ARTIFACT,
            expected_artifact_digest: &fixture.artifact_digest,
            expected_manifest_digest: &"00".repeat(32),
            granted_capabilities: &granted,
        };
        assert!(validate_installed_generation(&inputs).is_err());
    }

    #[test]
    fn undeclared_grant_fails_closed() {
        let fixture = Fixture::new(manifest());
        let granted = vec!["platform.notify".to_string()];
        assert!(validate_installed_generation(&fixture.inputs(ARTIFACT, &granted)).is_err());
    }

    #[test]
    fn unknown_capability_grant_fails_closed() {
        let fixture = Fixture::new(manifest());
        // A closed-set capability the manifest never declared.
        let undeclared = vec!["network.connect:evil.example".to_string()];
        assert!(validate_installed_generation(&fixture.inputs(ARTIFACT, &undeclared)).is_err());
        // A grant outside the closed set cannot be re-derived at all.
        let bogus = vec!["totally.not-a-capability".to_string()];
        assert!(validate_installed_generation(&fixture.inputs(ARTIFACT, &bogus)).is_err());
    }

    #[test]
    fn narrowed_grant_is_accepted_because_calls_deny_at_use() {
        let mut manifest = manifest();
        manifest.capabilities = Vec::new();
        let fixture = Fixture::new(manifest);
        let granted: Vec<String> = Vec::new();
        assert!(validate_installed_generation(&fixture.inputs(ARTIFACT, &granted)).is_ok());
    }

    #[test]
    fn oversized_manifest_fails_closed() {
        let mut manifest = manifest();
        manifest.raw_bytes_len = crate::manifest::MANIFEST_MAX_BYTES + 1;
        let fixture = Fixture::new(manifest);
        let granted = vec!["terminal.semantic-read".to_string()];
        assert!(validate_installed_generation(&fixture.inputs(ARTIFACT, &granted)).is_err());
    }

    /// Staged-tree fixture sharing the canonical encoding (`H-A` for trees).
    struct TreeFixture {
        tree_bytes: Vec<u8>,
        tree_digest: String,
        manifest_bytes: Vec<u8>,
        manifest_digest: String,
        declared: Vec<String>,
    }

    impl TreeFixture {
        fn new() -> Self {
            let files = vec![
                ("lua/init.lua", b"return {}\n" as &[u8]),
                ("lua/util.lua", b"local M = {}\n" as &[u8]),
            ];
            let tree_bytes = crate::source::canonical_tree_bytes(&files);
            let tree_digest = sha256_hex(&tree_bytes);
            let manifest_bytes = b"bitty-manifest-fixture\n".to_vec();
            let manifest_digest = sha256_hex(&manifest_bytes);
            Self {
                tree_bytes,
                tree_digest,
                manifest_bytes,
                manifest_digest,
                declared: vec!["terminal.semantic-read".to_string()],
            }
        }

        fn inputs<'a>(&'a self, tree: &'a [u8], granted: &'a [String]) -> StagedTreeInputs<'a> {
            StagedTreeInputs {
                tree_bytes: tree,
                expected_tree_digest: &self.tree_digest,
                manifest_canonical_bytes: &self.manifest_bytes,
                expected_manifest_digest: &self.manifest_digest,
                declared_capabilities: &self.declared,
                granted_capabilities: granted,
            }
        }
    }

    #[test]
    fn staged_tree_valid_generation_passes() {
        let fixture = TreeFixture::new();
        let granted = vec!["terminal.semantic-read".to_string()];
        assert!(
            validate_staged_tree_generation(&fixture.inputs(&fixture.tree_bytes, &granted)).is_ok()
        );
    }

    #[test]
    fn staged_tree_tampered_bytes_fail_closed() {
        let fixture = TreeFixture::new();
        let granted = vec!["terminal.semantic-read".to_string()];
        assert!(validate_staged_tree_generation(&fixture.inputs(b"tampered", &granted)).is_err());
    }

    #[test]
    fn staged_tree_tampered_manifest_binding_fails_closed() {
        let fixture = TreeFixture::new();
        let granted = vec!["terminal.semantic-read".to_string()];
        let inputs = StagedTreeInputs {
            tree_bytes: &fixture.tree_bytes,
            expected_tree_digest: &fixture.tree_digest,
            manifest_canonical_bytes: &fixture.manifest_bytes,
            expected_manifest_digest: &"00".repeat(32),
            declared_capabilities: &fixture.declared,
            granted_capabilities: &granted,
        };
        assert!(validate_staged_tree_generation(&inputs).is_err());
    }

    #[test]
    fn staged_tree_undeclared_grant_fails_closed() {
        let fixture = TreeFixture::new();
        let granted = vec!["platform.notify".to_string()];
        assert!(
            validate_staged_tree_generation(&fixture.inputs(&fixture.tree_bytes, &granted))
                .is_err()
        );
        let bogus = vec!["totally.not-a-capability".to_string()];
        assert!(
            validate_staged_tree_generation(&fixture.inputs(&fixture.tree_bytes, &bogus)).is_err()
        );
    }

    #[test]
    fn staged_tree_narrowed_grant_is_accepted() {
        let fixture = TreeFixture::new();
        let granted: Vec<String> = Vec::new();
        assert!(
            validate_staged_tree_generation(&fixture.inputs(&fixture.tree_bytes, &granted)).is_ok()
        );
    }

    #[test]
    fn staged_tree_digest_matches_shared_canonical_scheme() {
        // Old/new equivalence on the staged tree: the tree validator's `H-A`
        // is `sha256_hex(canonical_tree_bytes(files))`, exactly the shared
        // scheme — not a second digest construction.
        let files = vec![("b.lua", b"second" as &[u8]), ("a.lua", b"first" as &[u8])];
        let canonical = crate::source::canonical_tree_bytes(&files);
        assert_eq!(
            crate::source::digest_tree_files(&files),
            sha256_hex(&canonical)
        );
        assert_eq!(
            crate::source::digest_local_content(&files),
            sha256_hex(&canonical)
        );
        let fixture = TreeFixture::new();
        assert_eq!(fixture.tree_digest, sha256_hex(&fixture.tree_bytes));
    }
}
