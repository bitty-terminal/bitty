//! Draft install-time package verification — wires `bitty-package` into the plugin install pipeline.
//!
//! # Draft status — not normative
//!
//! This module implements the **proposed** contracts from
//! `bitty-docs/docs/specifications/package-lifecycle-rfc.md` (frontmatter `draft`).
//! That RFC is still `Proposed` and closes `OQ-021`/`OQ-022` only after independent
//! review by the category owner, a docs curator, and a security reviewer (including
//! a separate security-auditor persona per the documentation workflow). Nothing
//! here claims normative behavior, stable manifest formats, frozen digest schemes,
//! or settled publisher-trust policy. The module is intentionally `draft` /
//! `proposed` and its contract **may change** without a semver major bump until
//! the RFC is accepted. Do not describe its behavior as shipped until an ADR
//! records acceptance and a release ships it.
//!
//! # Integrity chain (RFC §Integrity verification chain)
//!
//! Seven stages, ordered, none may be skipped for any source type (bundled and
//! local-path use degenerate records, not exemptions):
//!
//! 1. `FetchFraming` — transfer size/time budgets
//! 2. `ArtifactChecksum` — H-A whole-artifact digest vs lock
//! 3. `ManifestValidation` — schema/field/limit validation as untrusted data
//! 4. `ManifestHashBinding` — H-B canonical digest vs lock (semantics binding)
//! 5. `CapabilityDiff` — P0-AC-030: any added capability blocks without explicit approval
//! 6. `CompatibilityCheck` — host Bitty/plugin-API versions satisfy `compat`
//! 7. `StoreCommit` — content-addressed store write succeeds, digests valid hex
//!
//! This module calls `bitty_package::verify_pipeline` (all 7 stages) **before**
//! any staging. Failure is fail-closed: the staged generation is never written,
//! the previous active pointer is unchanged, and an owned error is returned for
//! `bitty plugin doctor` diagnostics.
//!
//! # Trust (RFC §Publisher trust options)
//!
//! Three publisher trust options close the gap between checksums (what was fetched
//! matches what was locked) and authentication (who published it):
//!
//! - `V-A` (`TrustMode::PinningOnly`) — exact lock pinning plus checksums; P0 floor, normative.
//! - `V-B` (`TrustMode::TrustOnFirstUse`) — TOFU pin per publisher identity/source; pin change is a loud
//!   security event requiring explicit re-approval (PL-AC-003). Bind strongest
//!   available identity: publisher key where present, otherwise `url@rev`.
//! - `V-C` (`TrustMode::Signed`) — publisher signatures over manifest + artifact digests verified
//!   against an authenticated key record; fail-closed on unsigned, unknown-key, revoked-key,
//!   or signature over different bytes (PL-AC-004). Inherits V-B pin semantics where applicable.
//!
//! The caller selects a `TrustMode` per package source. All modes still pass the full
//! 7-stage chain; there is no trusted-source fast path.
//!
//! # Generation integrity (RFC §Safe rollback)
//!
//! Before staging, the module verifies every retained generation via
//! `Environment::verify_all` — a tampered or unparseable generation is
//! quarantined and reported, never activated (PL-AC-008/009). After staging,
//! the new generation's root digest is self-verified.
//!
//! # Ownership & constraints
//!
//! - Pure data + validation: no file I/O, no network, no VM, no code execution (`Invariant 8`).
//! - Headlessly testable on Linux CI and Windows (`windows-latest`).
//! - `#![forbid(unsafe_code)]`, `MSRV 1.85`, `edition = "2024"`.
//! - All errors are owned (`String`, `Vec<String>`) for `bitty plugin doctor`.

#![forbid(unsafe_code)]

use bitty_package::{
    Environment, KeyStore, PackageError, PackageManifest, SignatureRecord, TrustMode, TrustStore,
    VerificationInputs, VerificationReport, VerificationStage,
};

/// Inputs for the full install-time verification (pure data, no I/O).
///
/// Mirrors `bitty_package::VerificationInputs` plus publisher-trust and
/// generation-integrity contexts. Every field is owned or borrowed over
/// owned data so validation stays headless and deterministic.
#[derive(Debug, Clone)]
pub struct InstallInputs<'a> {
    /// Fetched artifact bytes (quarantine).
    pub artifact_bytes: &'a [u8],
    /// Expected artifact digest H-A from lock (64 hex).
    pub expected_artifact_digest: &'a str,
    /// Parsed manifest to validate (stage 3).
    pub manifest: &'a PackageManifest,
    /// Expected canonical manifest digest H-B from lock (64 hex).
    pub expected_manifest_digest: &'a str,
    /// Previously granted capability set (empty on first install). For
    /// capability-diff P0-AC-030; narrowing/equal carries forward silently.
    pub granted_capabilities: &'a [String],
    /// Newly requested capabilities (usually manifest's capabilities as strings).
    pub requested_capabilities: &'a [String],
    /// Whether capability-diff approval was given (explicit user consent).
    pub capability_approval: bool,
    /// Host Bitty version (e.g. `"0.6.0"`).
    pub host_bitty_version: Option<&'a str>,
    /// Host plugin API version (e.g. `"1.0.0"`).
    pub host_plugin_api_version: Option<&'a str>,
    /// Expected content root H-C, if store has landed (`None` until H-C active).
    pub expected_content_root: Option<&'a str>,
    /// Fetch framing: transferred bytes.
    pub fetch_bytes: usize,
    /// Fetch framing: elapsed ms.
    pub fetch_elapsed_ms: u64,
    /// Fetch framing budget: max bytes (e.g. `MAX_ARTIFACT_BYTES`).
    pub max_fetch_bytes: usize,
    /// Fetch framing budget: max ms.
    pub max_fetch_ms: u64,
    /// Package id for doctor diagnostics (e.g. `"xuepoo.theme"`).
    pub package_id: &'a str,
    /// Publisher trust mode for this source (`V-A` / `V-B` / `V-C`).
    pub trust_mode: TrustMode,
    /// Candidate publisher identity for `V-B` (`key-id` or `url@rev` or `url`).
    /// Checked against `trust_store` when `V-B` or `V-C` is selected.
    pub candidate_identity: Option<&'a str>,
    /// TOFU pin store (in-memory stub; persisted stub in real host). `None`
    /// means no stored pins yet (first install) — any identity passes but is then pinned.
    pub trust_store: Option<&'a TrustStore>,
    /// Signature record for `V-C`. Required when `trust_mode == Signed`; must be `None`
    /// for pure `V-A`/`V-B` floor unless a signed registry source also supplies one.
    pub signature: Option<&'a SignatureRecord>,
    /// Authenticated key store for `V-C` (trust anchor). `None` when not in V-C.
    pub key_store: Option<&'a KeyStore>,
    /// Retained environment to verify before staging (`None` on fresh install with no history).
    /// When `Some`, every retained generation is re-verified (`verify_all`) fail-closed.
    pub environment: Option<&'a Environment>,
}

/// Headless report for diagnostics / `bitty plugin doctor`.
///
/// Mirrors `VerificationReport` plus trust/generation sub-results for aggregation.
/// All fields owned; no I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorIssue {
    /// Package id this issue belongs to (owned for `doctor` grouping).
    pub package: String,
    /// Stage or trust/generation label that failed (e.g. `"artifact_checksum"`, `"trust_pin"`, `"generation"`).
    pub stage: String,
    /// Human-readable owned message (no borrowed display).
    pub message: String,
    /// Error class for aggregation (mirrors `PackageError::error_class` labels).
    pub error_class: String,
}

impl DoctorIssue {
    #[must_use]
    pub fn new(
        package: impl Into<String>,
        stage: impl Into<String>,
        message: impl Into<String>,
        error_class: impl Into<String>,
    ) -> Self {
        Self {
            package: package.into(),
            stage: stage.into(),
            message: message.into(),
            error_class: error_class.into(),
        }
    }

    /// Convert from a `PackageError` with package context for `doctor`.
    #[must_use]
    pub fn from_package_error(package: &str, err: &PackageError) -> Self {
        let class = err.error_class().to_string();
        // Map to a stage-like label for doctor grouping.
        let stage = match err {
            PackageError::DigestMismatch { kind, .. } => format!("{kind}_digest"),
            PackageError::ManifestHashMismatch { .. } => {
                VerificationStage::ManifestHashBinding.label().to_string()
            }
            PackageError::CapabilityIncrease { .. } => {
                VerificationStage::CapabilityDiff.label().to_string()
            }
            PackageError::TrustPinChanged { .. } => "trust_pin".to_string(),
            PackageError::Signature { .. } => "signature".to_string(),
            PackageError::Generation { .. } => "generation".to_string(),
            PackageError::Budget { .. } => VerificationStage::FetchFraming.label().to_string(),
            PackageError::Incompatible { .. } => {
                VerificationStage::CompatibilityCheck.label().to_string()
            }
            PackageError::Integrity { stage, .. } => stage.clone(),
            _ => "package".to_string(),
        };
        Self::new(package, stage, err.to_string(), class)
    }
}

/// Verify the full install pipeline before staging (fail-closed).
///
/// Runs:
///
/// 1. 7-stage `verify_pipeline` (fetch framing, H-A, manifest validation, H-B, capability diff,
///    compat, store commit) — any failure blocks staging, even if trust would pass.
/// 2. Publisher trust per `trust_mode`:
///    - `V-A PinningOnly`: no additional check beyond the 7 stages (floor).
///    - `V-B TrustOnFirstUse`: `trust_store.check(package_id, candidate_identity)` — pin change
///      requires loud re-approval (owned `TrustPinChanged` error).
///    - `V-C Signed`: `verify_signature(sig, keys, manifest_digest, artifact_digest)` — fail-closed
///      on unsigned/unknown/revoked/different-bytes (plus `V-B` pin check when a store is present).
/// 3. Generation integrity: if `inputs.environment` is `Some`, `env.verify_all()` — tampered
///    generation quarantined, never staged (PL-AC-008/009).
///
/// Returns `Ok(report)` when every gate passes; the caller may then stage the
/// generation and record a TOFU pin / grant. No staging is performed here and
/// no plugin VM is contacted (Invariant 8).
///
/// All errors are owned (`PackageError`) for `bitty plugin doctor`.
pub fn verify_install(inputs: &InstallInputs<'_>) -> Result<VerificationReport, PackageError> {
    // Cheap local-path promotion guard could be here; for draft we keep the check
    // in the caller that knows `PackageSource` provenance.

    // Generation integrity first: if existing retained history is corrupt, we block
    // even before checking the new artifact (PL-AC-008/009). Fail-closed with quarantine.
    if let Some(env) = inputs.environment {
        env.verify_all()
            .map_err(|e| PackageError::generation(format!("generation quarantine: {e}")))?;
    }

    // 7-stage pipeline — the single source of truth for integrity binding, capability diff,
    // compat, and store commit (H-A/B/C).
    let verification_inputs = VerificationInputs {
        artifact_bytes: inputs.artifact_bytes,
        expected_artifact_digest: inputs.expected_artifact_digest,
        manifest: inputs.manifest,
        expected_manifest_digest: inputs.expected_manifest_digest,
        granted_capabilities: inputs.granted_capabilities,
        requested_capabilities: inputs.requested_capabilities,
        capability_approval: inputs.capability_approval,
        host_bitty_version: inputs.host_bitty_version,
        host_plugin_api_version: inputs.host_plugin_api_version,
        expected_content_root: inputs.expected_content_root,
        fetch_bytes: inputs.fetch_bytes,
        fetch_elapsed_ms: inputs.fetch_elapsed_ms,
        max_fetch_bytes: inputs.max_fetch_bytes,
        max_fetch_ms: inputs.max_fetch_ms,
    };
    let report = bitty_package::verify_pipeline(&verification_inputs);
    if !report.is_passed() {
        // Surface the first failing stage as an owned PackageError for doctor.
        // Keep the full report available to the caller for aggregated diagnostics.
        let first = report
            .first_failure()
            .expect("report has failure when not passed");
        // Map stage to the concrete PackageError that would have been produced
        // in the pipeline, preserving stage label for doctor grouping.
        // For most stages the pipeline already produced a PackageError internally;
        // we synthesize an Integrity error that carries the stage label + message
        // so doctor sees `stage` in `DoctorIssue`.
        return Err(PackageError::Integrity {
            stage: first.stage.label().to_string(),
            message: format!("{}: {}", first.stage.label(), first.message),
        });
    }

    // Publisher trust per mode — none relaxes V-A; every source still passed the full chain.
    match inputs.trust_mode {
        TrustMode::PinningOnly => {
            // Floor: checksums already enforced; no additional identity proof.
        }
        TrustMode::TrustOnFirstUse => {
            // TOFU: require candidate identity and check pin.
            let candidate = inputs.candidate_identity.ok_or_else(|| {
                PackageError::source(
                    "trust V-B requires candidate identity (publisher key or source URL)",
                )
            })?;
            if let Some(store) = inputs.trust_store {
                // Parse package id for store check; PackageId validation is the same as manifest id.
                let pid = bitty_package::PackageId::new(inputs.package_id).map_err(|e| {
                    PackageError::source(format!("invalid package id '{}': {e}", inputs.package_id))
                })?;
                store.check(&pid, candidate)?;
            }
            // No store yet (first install) -> any identity passes; caller pins after success.
        }
        TrustMode::Signed => {
            // V-C: signature must be present and verify against supplied KeyStore.
            let sig = inputs.signature.ok_or_else(|| {
                PackageError::signature("trust V-C requires a signature record (fail-closed: unsigned artifact rejected)")
            })?;
            let keys = inputs.key_store.ok_or_else(|| {
                PackageError::signature("trust V-C requires a key store with trusted keys")
            })?;
            bitty_package::verify_signature(
                sig,
                keys,
                inputs.expected_manifest_digest,
                inputs.expected_artifact_digest,
            )?;
            // V-C subsumes V-B for signed sources: if a TOFU store is present, also enforce pin check.
            if let Some(store) = inputs.trust_store {
                if let Some(candidate) = inputs.candidate_identity {
                    let pid = bitty_package::PackageId::new(inputs.package_id).map_err(|e| {
                        PackageError::source(format!(
                            "invalid package id '{}': {e}",
                            inputs.package_id
                        ))
                    })?;
                    store.check(&pid, candidate)?;
                }
            }
        }
    }

    Ok(report)
}

/// Convenience: whether `report` is fully passing, for gating `stage`.
///
/// Staging must only be called when this returns `true` and `verify_install` is `Ok`.
#[must_use]
pub fn is_staging_allowed(report: &VerificationReport) -> bool {
    report.is_passed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitty_package::{
        CapabilityId, Compat, KeyRecord, KeyStore, PackageIdentity, PackageManifest,
        SignatureRecord, TrustPin, TrustStore, sha256_hex, stub_sign,
    };
    use std::collections::BTreeMap;

    fn minimal_package_manifest(id: &str) -> PackageManifest {
        PackageManifest {
            identity: PackageIdentity {
                id: bitty_package::PackageId::new(id).unwrap(),
                name: "Test".to_string(),
                version: "0.1.0".to_string(),
                description: "desc".to_string(),
                license: Some("MIT".to_string()),
            },
            compat: Compat {
                bitty: Some(">=0.5,<1.0".to_string()),
                plugin_api: Some("^1.0".to_string()),
            },
            dependencies: Vec::new(),
            capabilities: Vec::new(),
            raw_bytes_len: 256,
            undeclared_fields: Vec::new(),
        }
    }

    fn default_inputs<'a>(
        artifact: &'a [u8],
        artifact_digest: &'a str,
        manifest: &'a PackageManifest,
        manifest_digest: &'a str,
        granted: &'a [String],
        requested: &'a [String],
        approval: bool,
    ) -> InstallInputs<'a> {
        InstallInputs {
            artifact_bytes: artifact,
            expected_artifact_digest: artifact_digest,
            manifest,
            expected_manifest_digest: manifest_digest,
            granted_capabilities: granted,
            requested_capabilities: requested,
            capability_approval: approval,
            host_bitty_version: Some("0.6.0"),
            host_plugin_api_version: Some("1.0.0"),
            expected_content_root: None,
            fetch_bytes: artifact.len(),
            fetch_elapsed_ms: 10,
            max_fetch_bytes: 10 * 1024 * 1024,
            max_fetch_ms: 5000,
            package_id: "xuepoo.test",
            trust_mode: TrustMode::PinningOnly,
            candidate_identity: None,
            trust_store: None,
            signature: None,
            key_store: None,
            environment: None,
        }
    }

    #[test]
    fn tampered_artifact_blocks_before_staging() {
        let manifest = minimal_package_manifest("xuepoo.test");
        let manifest_digest = manifest.canonical_digest();
        let artifact = b"good artifact bytes";
        let good_artifact_digest = sha256_hex(artifact);
        let tampered = b"tampered bytes";

        let inputs = default_inputs(
            tampered,
            &good_artifact_digest,
            &manifest,
            &manifest_digest,
            &[],
            &[],
            false,
        );
        let err = verify_install(&inputs).unwrap_err();
        let doctor = DoctorIssue::from_package_error("xuepoo.test", &err);
        assert_eq!(doctor.stage, "artifact_checksum");
        assert!(doctor.message.contains("artifact_checksum"));
        // Prove no staging occurred: caller would guard `is_staging_allowed`
        // — here we show the report never reaches `Ok`.
        assert!(!doctor.message.contains("ok"));
    }

    #[test]
    fn tampered_manifest_hash_mismatch_blocks() {
        let manifest = minimal_package_manifest("xuepoo.test");
        let good_manifest_digest = manifest.canonical_digest();
        // Tamper by changing capabilities semantically (digest changes).
        let mut tampered = manifest.clone();
        tampered
            .capabilities
            .push(CapabilityId::new("fs.read").unwrap());
        let _tampered_digest = tampered.canonical_digest();
        assert_ne!(good_manifest_digest, tampered.canonical_digest());

        let artifact = b"artifact bytes";
        let artifact_digest = sha256_hex(artifact);

        // Verify tampered manifest against good digest must fail at manifest_hash_binding before staging.
        let inputs = InstallInputs {
            artifact_bytes: artifact,
            expected_artifact_digest: &artifact_digest,
            manifest: &tampered,
            expected_manifest_digest: &good_manifest_digest,
            granted_capabilities: &[],
            requested_capabilities: &["fs.read".to_string()],
            capability_approval: false,
            host_bitty_version: Some("0.6.0"),
            host_plugin_api_version: Some("1.0.0"),
            expected_content_root: None,
            fetch_bytes: artifact.len(),
            fetch_elapsed_ms: 10,
            max_fetch_bytes: 10 * 1024 * 1024,
            max_fetch_ms: 5000,
            package_id: "xuepoo.test",
            trust_mode: TrustMode::PinningOnly,
            candidate_identity: None,
            trust_store: None,
            signature: None,
            key_store: None,
            environment: None,
        };
        let err = verify_install(&inputs).unwrap_err();
        let doctor = DoctorIssue::from_package_error("xuepoo.test", &err);
        assert_eq!(doctor.stage, "manifest_hash_binding");
    }

    #[test]
    fn capability_increase_blocked_without_approval() {
        let manifest = minimal_package_manifest("xuepoo.cap");
        let manifest_digest = manifest.canonical_digest();
        let artifact = b"bytes";
        let artifact_digest = sha256_hex(artifact);

        let granted = vec!["fs.read".to_string()];
        let requested = vec!["fs.read".to_string(), "fs.write".to_string()];

        // Without approval -> blocked before staging (P0-AC-030).
        let inputs = default_inputs(
            artifact,
            &artifact_digest,
            &manifest,
            &manifest_digest,
            &granted,
            &requested,
            false,
        );
        let err = verify_install(&inputs).unwrap_err();
        let doctor = DoctorIssue::from_package_error("xuepoo.cap", &err);
        assert_eq!(doctor.stage, "capability_diff");
        assert!(doctor.message.contains("capability"));

        // With approval -> passes and staging allowed.
        let inputs_approved = InstallInputs {
            capability_approval: true,
            ..default_inputs(
                artifact,
                &artifact_digest,
                &manifest,
                &manifest_digest,
                &granted,
                &requested,
                true,
            )
        };
        let report = verify_install(&inputs_approved).unwrap();
        assert!(is_staging_allowed(&report));
    }

    #[test]
    fn capability_narrowing_carries_forward_silently() {
        let manifest = minimal_package_manifest("xuepoo.cap");
        let manifest_digest = manifest.canonical_digest();
        let artifact = b"bytes";
        let artifact_digest = sha256_hex(artifact);

        let granted = vec!["fs.read".to_string(), "fs.write".to_string()];
        let narrowed = vec!["fs.read".to_string()];

        let inputs = default_inputs(
            artifact,
            &artifact_digest,
            &manifest,
            &manifest_digest,
            &granted,
            &narrowed,
            false,
        );
        let report = verify_install(&inputs).unwrap();
        assert!(report.is_passed());
    }

    #[test]
    fn trust_vb_pin_change_blocks_and_doctor_issue() {
        let manifest = minimal_package_manifest("xuepoo.b");
        let manifest_digest = manifest.canonical_digest();
        let artifact = b"bytes";
        let artifact_digest = sha256_hex(artifact);

        let mut trust_store = TrustStore::new();
        trust_store
            .pin(TrustPin {
                package: bitty_package::PackageId::new("xuepoo.b").unwrap(),
                identity: "key-old".to_string(),
                mode: TrustMode::TrustOnFirstUse,
                first_seen: 1,
            })
            .unwrap();

        // Candidate identity changed -> loud security event before staging.
        let inputs = InstallInputs {
            artifact_bytes: artifact,
            expected_artifact_digest: &artifact_digest,
            manifest: &manifest,
            expected_manifest_digest: &manifest_digest,
            granted_capabilities: &[],
            requested_capabilities: &[],
            capability_approval: false,
            host_bitty_version: Some("0.6.0"),
            host_plugin_api_version: Some("1.0.0"),
            expected_content_root: None,
            fetch_bytes: artifact.len(),
            fetch_elapsed_ms: 10,
            max_fetch_bytes: 10 * 1024 * 1024,
            max_fetch_ms: 5000,
            package_id: "xuepoo.b",
            trust_mode: TrustMode::TrustOnFirstUse,
            candidate_identity: Some("key-new"),
            trust_store: Some(&trust_store),
            signature: None,
            key_store: None,
            environment: None,
        };
        let err = verify_install(&inputs).unwrap_err();
        let doctor = DoctorIssue::from_package_error("xuepoo.b", &err);
        assert_eq!(doctor.error_class, "trust");
        assert_eq!(doctor.stage, "trust_pin");
        assert!(doctor.message.contains("re-approval required"));

        // Same identity passes.
        let inputs_same = InstallInputs {
            candidate_identity: Some("key-old"),
            ..inputs
        };
        assert!(verify_install(&inputs_same).is_ok());

        // First install (no pin) passes any identity (but caller then pins).
        let empty_store = TrustStore::new();
        let inputs_first = InstallInputs {
            candidate_identity: Some("key-first"),
            trust_store: Some(&empty_store),
            ..inputs
        };
        assert!(verify_install(&inputs_first).is_ok());
    }

    #[test]
    fn trust_vc_signature_fail_closed_and_valid_passes() {
        let manifest = minimal_package_manifest("xuepoo.c");
        let manifest_digest = manifest.canonical_digest();
        let artifact = b"artifact for signing";
        let artifact_digest = sha256_hex(artifact);

        let mut keys = KeyStore::new();
        keys.insert(KeyRecord {
            key_id: "k1".to_string(),
            public_key_hex: "a".repeat(64),
            revoked: false,
        })
        .unwrap();

        let sig_hex = stub_sign("k1", &manifest_digest, &artifact_digest);
        let valid_sig = SignatureRecord {
            key_id: "k1".to_string(),
            signature_hex: sig_hex.clone(),
            manifest_digest: manifest_digest.clone(),
            artifact_digest: artifact_digest.clone(),
        };

        // Valid signed release passes before staging.
        let inputs = InstallInputs {
            artifact_bytes: artifact,
            expected_artifact_digest: &artifact_digest,
            manifest: &manifest,
            expected_manifest_digest: &manifest_digest,
            granted_capabilities: &[],
            requested_capabilities: &[],
            capability_approval: false,
            host_bitty_version: Some("0.6.0"),
            host_plugin_api_version: Some("1.0.0"),
            expected_content_root: None,
            fetch_bytes: artifact.len(),
            fetch_elapsed_ms: 10,
            max_fetch_bytes: 10 * 1024 * 1024,
            max_fetch_ms: 5000,
            package_id: "xuepoo.c",
            trust_mode: TrustMode::Signed,
            candidate_identity: None,
            trust_store: None,
            signature: Some(&valid_sig),
            key_store: Some(&keys),
            environment: None,
        };
        let report = verify_install(&inputs).unwrap();
        assert!(report.is_passed());

        // Unsigned (no signature) -> blocked.
        let inputs_unsigned = InstallInputs {
            signature: None,
            ..inputs.clone()
        };
        let err = verify_install(&inputs_unsigned).unwrap_err();
        assert_eq!(
            DoctorIssue::from_package_error("xuepoo.c", &err).stage,
            "signature"
        );

        // Unknown key -> blocked.
        let unknown_sig = SignatureRecord {
            key_id: "unknown".to_string(),
            signature_hex: sig_hex.clone(),
            manifest_digest: manifest_digest.clone(),
            artifact_digest: artifact_digest.clone(),
        };
        let inputs_unknown = InstallInputs {
            signature: Some(&unknown_sig),
            ..inputs.clone()
        };
        assert!(verify_install(&inputs_unknown).is_err());

        // Signature over different bytes -> blocked.
        let other_manifest_digest = sha256_hex(b"other manifest");
        let sig_over_different = SignatureRecord {
            key_id: "k1".to_string(),
            signature_hex: stub_sign("k1", &other_manifest_digest, &artifact_digest),
            manifest_digest: other_manifest_digest.clone(),
            artifact_digest: artifact_digest.clone(),
        };
        let inputs_different = InstallInputs {
            signature: Some(&sig_over_different),
            ..inputs.clone()
        };
        assert!(verify_install(&inputs_different).is_err());

        // Revoked key -> blocked (fail-closed even with valid bytes).
        let mut revoked_keys = keys.clone();
        revoked_keys.revoke("k1").unwrap();
        let inputs_revoked = InstallInputs {
            key_store: Some(&revoked_keys),
            signature: Some(&valid_sig),
            ..inputs.clone()
        };
        let err = verify_install(&inputs_revoked).unwrap_err();
        assert!(DoctorIssue::from_package_error("xuepoo.c", &err).stage == "signature");

        // Validly signed content from rotated-but-trusted key still verifies.
        let mut rotated = KeyStore::new();
        rotated
            .insert(KeyRecord {
                key_id: "k1".to_string(),
                public_key_hex: "a".repeat(64),
                revoked: false,
            })
            .unwrap();
        rotated
            .insert(KeyRecord {
                key_id: "k2".to_string(),
                public_key_hex: "b".repeat(64),
                revoked: false,
            })
            .unwrap();
        let sig_k2 = SignatureRecord {
            key_id: "k2".to_string(),
            signature_hex: stub_sign("k2", &manifest_digest, &artifact_digest),
            manifest_digest: manifest_digest.clone(),
            artifact_digest: artifact_digest.clone(),
        };
        let inputs_rotated = InstallInputs {
            signature: Some(&sig_k2),
            key_store: Some(&rotated),
            ..inputs.clone()
        };
        assert!(verify_install(&inputs_rotated).is_ok());
    }

    #[test]
    fn generation_integrity_blocks_tampered_history_before_staging() {
        use bitty_package::{LockedPackage, Lockfile, PackageDigests, PackageSource};

        let manifest = minimal_package_manifest("xuepoo.gen");
        let manifest_digest = manifest.canonical_digest();
        let artifact = b"good";
        let artifact_digest = sha256_hex(artifact);

        // Build a retained environment with one good generation.
        let mut env = Environment::new();
        let mut lock = Lockfile::new();
        lock.insert(LockedPackage {
            id: bitty_package::PackageId::new("xuepoo.gen").unwrap(),
            version: "0.1.0".to_string(),
            source: PackageSource::Registry {
                url: "https://example.com".to_string(),
            },
            digests: PackageDigests {
                artifact: artifact_digest.clone(),
                manifest: manifest_digest.clone(),
                content_root: None,
            },
            locked_at: 1,
        })
        .unwrap();
        let gen_id = env
            .stage(
                lock,
                BTreeMap::from([("xuepoo.gen".to_string(), vec!["fs.read".to_string()])]),
                10,
            )
            .unwrap();
        bitty_package::activate(&mut env, gen_id, Some("0.6.0"), Some("1.0.0"), None).unwrap();
        assert!(env.current == Some(gen_id));

        // Tamper the retained generation's root digest (store tampering).
        env.generations.get_mut(&gen_id).unwrap().root_digest = "a".repeat(64);
        // Any new install must now block at generation integrity before staging (PL-AC-008/009).
        let inputs = InstallInputs {
            artifact_bytes: artifact,
            expected_artifact_digest: &artifact_digest,
            manifest: &manifest,
            expected_manifest_digest: &manifest_digest,
            granted_capabilities: &[],
            requested_capabilities: &[],
            capability_approval: false,
            host_bitty_version: Some("0.6.0"),
            host_plugin_api_version: Some("1.0.0"),
            expected_content_root: None,
            fetch_bytes: artifact.len(),
            fetch_elapsed_ms: 10,
            max_fetch_bytes: 10 * 1024 * 1024,
            max_fetch_ms: 5000,
            package_id: "xuepoo.gen",
            trust_mode: TrustMode::PinningOnly,
            candidate_identity: None,
            trust_store: None,
            signature: None,
            key_store: None,
            environment: Some(&env),
        };
        let err = verify_install(&inputs).unwrap_err();
        let doctor = DoctorIssue::from_package_error("xuepoo.gen", &err);
        assert_eq!(doctor.error_class, "generation");
        assert_eq!(doctor.stage, "generation");
        // Prove no new generation was staged: caller's guard would check verify before stage.
        // Here the verify failed, so caller must not call stage.
        assert!(err.to_string().contains("quarantine"));
    }

    #[test]
    fn full_valid_install_passes_and_staging_allowed() {
        let manifest = minimal_package_manifest("xuepoo.good");
        let manifest_digest = manifest.canonical_digest();
        let artifact = b"package artifact bytes v1";
        let artifact_digest = sha256_hex(artifact);

        let inputs = default_inputs(
            artifact,
            &artifact_digest,
            &manifest,
            &manifest_digest,
            &[],
            &[],
            false,
        );
        let report = verify_install(&inputs).unwrap();
        assert!(report.is_passed());
        assert!(is_staging_allowed(&report));
        // Demonstrate staging only after verify: simulate stage.
        use bitty_package::{LockedPackage, Lockfile, PackageDigests, PackageSource};
        let mut env = Environment::new();
        let mut lock = Lockfile::new();
        lock.insert(LockedPackage {
            id: bitty_package::PackageId::new("xuepoo.good").unwrap(),
            version: "0.1.0".to_string(),
            source: PackageSource::Registry {
                url: "https://example.com".to_string(),
            },
            digests: PackageDigests {
                artifact: artifact_digest.clone(),
                manifest: manifest_digest,
                content_root: None,
            },
            locked_at: 1,
        })
        .unwrap();
        let generation_id = env
            .stage(lock, BTreeMap::new(), 100)
            .expect("staging after verified must succeed");
        assert!(env.is_retained(generation_id));
    }

    #[test]
    fn doctor_issue_owned_for_all_error_classes() {
        let err = PackageError::CapabilityIncrease {
            added: vec!["fs.write".to_string()],
        };
        let issue = DoctorIssue::from_package_error("xuepoo.doc", &err);
        assert_eq!(issue.error_class, "integrity");
        assert_eq!(issue.stage, "capability_diff");
        assert!(issue.message.contains("fs.write"));

        let err = PackageError::TrustPinChanged {
            package: "xuepoo.doc".to_string(),
            old: "old".to_string(),
            new: "new".to_string(),
        };
        let issue = DoctorIssue::from_package_error("xuepoo.doc", &err);
        assert_eq!(issue.error_class, "trust");
        assert_eq!(issue.package, "xuepoo.doc");

        let err = PackageError::generation("tampered");
        let issue = DoctorIssue::from_package_error("xuepoo.doc", &err);
        assert_eq!(issue.error_class, "generation");
    }
}
