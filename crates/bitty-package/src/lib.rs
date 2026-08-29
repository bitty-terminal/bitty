//! `bitty-package`: draft package lifecycle for Bitty.
//!
//! # Draft status — not normative
//!
//! This crate implements the **proposed** contracts from
//! `bitty-docs/docs/specifications/package-lifecycle-rfc.md`.
//! That RFC is still `Proposed` (frontmatter `draft`) and closes `OQ-021`
//! and `OQ-022` only if it is adopted after independent review by the
//! category owner, a docs curator, and a security reviewer (including a
//! separate security-auditor persona per the documentation workflow).
//! Nothing here claims normative behavior, stable manifest or lockfile
//! formats, frozen digest schemes, or settled publisher-trust policy.
//! The crate is intentionally `draft` / `proposed` and its contract
//! **may change** without a semver major bump until the RFC is accepted.
//! Do not describe its behavior as shipped until an ADR records acceptance
//! and a release ships it.
//!
//! The RFC depends on normative security requirements in
//! `bitty-docs/docs/security/overview.md`, `threat-model.md`, and
//! `p0-acceptance-criteria.md`. This crate only proposes mechanisms beneath
//! them — it never weakens `Invariant 7` (bounded inputs), `Invariant 8`
//! (no package code execution on install), or `P0-AC-027..030`.
//!
//! # What this crate owns
//!
//! - Owned package manifest and lockfile types with triple-digest binding
//!   (`H-A` artifact digest, `H-B` canonical manifest digest, `H-C` Merkle
//!   root candidate).
//! - The 6-state lifecycle `Discovered -> Fetched -> Verified -> Staged`
//!   `-> Activated -> Retained` with fail-closed gated transitions; `Restored`
//!   is modeled as re-activating a retained generation.
//! - The 7-stage integrity verification chain applied identically to every
//!   source type (no trusted-source fast path).
//! - Publisher trust options `V-A` (pinning), `V-B` (TOFU), `V-C` (signed)
//!   with explicit re-approval semantics.
//! - Staged activation as one atomic pointer switch plus retained
//!   generations; safe rollback (full and per-plugin) with symmetric
//!   capability gates.
//! - Local-path development packages with visibly different trust semantics
//!   and drift detection.
//! - Closed constraint grammar (`^`, `~`, comparators, comma intersection)
//!   and deterministic resolver with single-version convergence, yank
//!   advisory and prerelease opt-in per follow-up RFC (see `version`,
//!   `requirement`, `resolver`).
//!
//! # What this crate does NOT do
//!
//! - No file I/O, no network, no process spawning, no plugin VM contact.
//! - No package code is ever executed (Invariant 8). Installation spans
//!   discovery through staging; activation is a separate transaction owned
//!   by the package-manager service, never re-implemented by CLI adapters
//!   or performed by the plugin host. Tests assert `may_execute_code`
//!   only for `Activated`.
//! - No runtime or platform coupling (no `winit`, `wgpu`, `portable-pty`).
//!   The crate is headlessly testable on Linux CI and the `windows-latest`
//!   job.
//! - No registry service, key-directory, or revocation infrastructure beyond
//!   the in-memory stub stores that demonstrate the V-B/V-C contracts.
//! - Side-by-side coexisting versions remain deferred per follow-up RFC;
//!   the resolver converges to a single version per ID and fails on diamond
//!   conflicts (see `resolver`).
//!
//! # Pipeline (candidate, RFC §Lifecycle + §Integrity chain)
//!
//! ```text
//! discovered --fetch--> fetched --integrity--> verified --store--> staged --txn--> activated
//!                                                        \                 \--> retained --(prune)-->
//!                                                         \--> (rollback) ---> activated (restored)
//! ```
//!
//! - `discovered`: source declares `id/version`; nothing trusted.
//! - `fetched`: bytes in quarantine, budgets enforced (stage 1).
//! - `verified`: stages 2–6 pass against the lock record (H-A, manifest
//!   validation, H-B binding, capability diff, compat).
//! - `staged`: verified content sits in the package store under digests; the
//!   new lock is persisted only after the store commit succeeds (stage 7).
//! - `activated`: one atomic switch makes the staged generation the active
//!   environment (phases: `preflight -> quiesce -> commit -> wake -> confirm`).
//! - `retained`: superseded generations kept up to `N` for rollback.
//!
//! # RFC section mapping
//!
//! | RFC section | Module(s) | Key items |
//! |-------------|-----------|-----------|
//! | Lifecycle overview — 6 states | `lifecycle` | [`lifecycle::PackageState`] 6-state enum, [`lifecycle::can_transition`] gate table, [`lifecycle::LifecycleRegistry`] fail-closed registry |
//! | Integrity verification chain — 7 stages | `integrity` | [`integrity::VerificationStage`] ordered 7-stage enum, [`integrity::verify_pipeline`] fan-in, [`integrity::sha256_hex`] SHA-256 hex, `H-A`/`H-B`/`H-C` binding |
//! | Manifest hashing schemes H-A/B/C | `manifest`, `lockfile`, `integrity` | [`manifest::PackageManifest::canonical_bytes`] deterministic canonical form (`bitty-manifest-v1`), [`manifest::PackageManifest::canonical_digest`] `H-B`, [`lockfile::PackageDigests`] triple, [`integrity::verify_artifact_checksum`] `H-A`, `content_root` `H-C` |
//! | Publisher trust options V-A/B/C | `trust` | [`trust::TrustMode`] `V-A`/`V-B`/`V-C`, [`trust::TrustStore`] TOFU pin with `check` -> `TrustPinChanged`, [`trust::KeyStore`] + [`trust::verify_signature`] fail-closed `V-C`, `stub_sign` helper |
//! | Local-path development packages | `source` | [`source::PackageSource::LocalPath`] degenerate record, [`source::digest_local_content`] + [`source::check_local_path_drift`] drift detection, [`source::ensure_no_promotion_without_chain`] provenance separation |
//! | Staged activation lifecycle — phases + S1/S2 | `activation` | [`activation::ActivationPhase`] 5-phase txn, [`activation::Environment`] generation ring with atomic `current` pointer (`S1` rename/`S2` generations recommendation: generations history + one atomic select), [`activation::activate`] fault-injection per phase, all-or-nothing commit semantics |
//! | Safe rollback — retained environments | `activation` | [`activation::Generation`] immutable entry + `verify_integrity` self-verification, [`activation::RetentionPolicy`] `N=2` (+current) bounded prune, never removes current |
//! | Safe rollback — operations | `activation` | [`activation::rollback_full`] + [`activation::rollback_per_plugin`] same staged txn in reverse, capability gates symmetric via `integrity::check_capability_diff`, safe-mode independent (no third-party load) |
//! | Verification criteria PL-AC-001..010 | `integrity`, `trust`, `source`, `activation` | `PL-AC-001` multi-digest binding via triple digest + independent tamper detection; `PL-AC-002` canonical determinism (sorted keys, cross-platform `sha256_hex`); `PL-AC-003` pin-change loud event (`TrustPinChanged`); `PL-AC-004` signature fail-closed; `PL-AC-005` drift detection; `PL-AC-006` atomic staged switch (pre-commit failure leaves pointer, mixed-state impossible); `PL-AC-007` wake/confirm restore; `PL-AC-008` deterministic rollback digest equality; `PL-AC-009` retention bounds; `PL-AC-010` symmetric capability gates on redo |
//! | Constraint grammar (closed, 128B) | `version`, `requirement` | [`version::Version`], [`requirement::VersionReq`] with caret/tilde expansion, comparator intersection, `*`/`||` denial, prerelease opt-in check |
//! | Resolver determinism & convergence | `resolver` | [`resolver::resolve`] pure `(manifest, index)` with canonical sorting, single-version per ID, conflict report, yank/prerelease filtering, budgets 64 edges/pkg and 256 pkgs, deterministic digest |
//! | Yank & prerelease lifecycle (PLF-AC-004) | `resolver` | Yanked excluded for new resolves, preserved for locked `resolve_preserving_locked` with `yanked (locked)` warning; prerelease excluded unless every edge opts in via `prerelease=true` or same-core `X.Y.Z-prerelease` comparator |
//! | Single-version convergence, side-by-side deferred | `resolver` | One version per ID in active generation; resolver never merges two coexisting versions, fails with conflict naming both edges |
//! | Residual open items | docs only | Documented honestly: manifest/lock format versions and canonical encoding spec, key enrollment/rotation/freshness, registry attestation, bundled packages |
//!
//! # Ownership rules (ADR-0003 / ADR-0004)
//!
//! - **Depends on:** nothing (pure `std`). No workspace-crate dependencies.
//! - **No third-party dependencies** (pure `std` plus vendored SHA-256).
//!   The RFC's crypto is stubbed deterministically; real signatures will
//!   land with the `bitty-docs` key-management design.
//! - **Never holds** GPU objects, window handles, PTY file descriptors, or
//!   internal Rust hot-path objects. It is pure data + validation.
//! - **`#![forbid(unsafe_code)]`** at crate and workspace level; `MSRV 1.85`,
//!   `edition = "2024"`.
//! - All structures are owned (`String`, `Vec`, `BTreeMap` …), never `&str` —
//!   so manifests, locks, digests, and generation entries are cloneable,
//!   comparable, and sendable without lifetimes.
//! - `bitty-package` is `publish = false` at the workspace level today;
//!   publication will track RFC acceptance.

#![forbid(unsafe_code)]

pub mod activation;
pub mod error;
pub mod integrity;
pub mod lifecycle;
pub mod lockfile;
pub mod manifest;
pub mod requirement;
pub mod resolver;
pub mod source;
pub mod trust;
pub mod version;

pub use activation::{
    ActivationPhase, ActivationReport, Environment, Generation, RetentionPolicy, activate,
    rollback_full, rollback_per_plugin,
};
pub use error::{ErrorClass, PackageError};
pub use integrity::{
    MAX_ARTIFACT_BYTES, VerificationInputs, VerificationReport, VerificationStage, capability_diff,
    check_capability_diff, check_compatibility, check_fetch_framing, is_valid_hex_digest,
    sha256_hex, validate_hex_digest, verify_artifact_checksum, verify_manifest,
    verify_manifest_hash_binding, verify_pipeline, verify_store_commit,
};
pub use lifecycle::{
    LifecycleRegistry, PackageLifecycle, PackageState, can_transition, successors,
};
pub use lockfile::{LOCKFILE_VERSION, LockedPackage, Lockfile, PackageDigests};
pub use manifest::{
    CapabilityId, Compat, MANIFEST_MAX_BYTES, MAX_CAPABILITIES, MAX_DEPENDENCIES,
    PackageDependency, PackageId, PackageIdentity, PackageManifest,
};
pub use requirement::{Comparator, ComparatorOp, MAX_REQUIREMENT_LEN, VersionReq};
pub use resolver::{
    IndexEntry, MAX_EDGES_PER_PACKAGE, MAX_PACKAGES_PER_RESOLUTION, PackageIndex, Resolution,
    ResolvedPackage, resolve, resolve_preserving_locked,
};
pub use source::{
    PackageSource, check_local_path_drift, digest_local_content, ensure_no_promotion_without_chain,
};
pub use trust::{
    KeyRecord, KeyStore, SignatureRecord, TrustMode, TrustPin, TrustStore, stub_sign,
    verify_signature,
};
pub use version::{MAX_VERSION_LEN, Version};

#[cfg(test)]
mod integration_tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn end_to_end_add_verify_stage_activate() {
        // Simulate `add` discovery through staging, then activation.
        let pid = PackageId::new("xuepoo.theme").unwrap();
        let manifest = PackageManifest {
            identity: PackageIdentity {
                id: pid.clone(),
                name: "Theme".to_string(),
                version: "0.1.0".to_string(),
                description: "A theme".to_string(),
                license: Some("MIT".to_string()),
            },
            compat: Compat {
                bitty: Some(">=0.5,<1.0".to_string()),
                plugin_api: Some("^1.0".to_string()),
            },
            dependencies: Vec::new(),
            capabilities: vec![CapabilityId::new("fs.read").unwrap()],
            raw_bytes_len: 256,
            undeclared_fields: Vec::new(),
        };
        manifest.validate().unwrap();

        // Lifecycle: discovered -> fetched -> verified -> staged.
        let mut lifecycles = LifecycleRegistry::new();
        lifecycles.declare(pid.clone()).unwrap();
        lifecycles.transition(&pid, PackageState::Fetched).unwrap();
        // Integrity gate.
        let artifact = b"pkg artifact bytes";
        let artifact_digest = sha256_hex(artifact);
        let manifest_digest = manifest.canonical_digest();
        let inputs = VerificationInputs {
            artifact_bytes: artifact,
            expected_artifact_digest: &artifact_digest,
            manifest: &manifest,
            expected_manifest_digest: &manifest_digest,
            granted_capabilities: &[],
            requested_capabilities: &["fs.read".to_string()],
            capability_approval: true,
            host_bitty_version: Some("0.6.0"),
            host_plugin_api_version: Some("1.0.0"),
            expected_content_root: None,
            fetch_bytes: artifact.len(),
            fetch_elapsed_ms: 10,
            max_fetch_bytes: MAX_ARTIFACT_BYTES,
            max_fetch_ms: 5000,
        };
        let report = verify_pipeline(&inputs);
        assert!(report.is_passed(), "verification failed: {report:?}");
        lifecycles.transition(&pid, PackageState::Verified).unwrap();
        lifecycles.transition(&pid, PackageState::Staged).unwrap();

        // Lock.
        let mut lock = Lockfile::new();
        lock.insert(LockedPackage {
            id: pid.clone(),
            version: "0.1.0".to_string(),
            source: PackageSource::Registry {
                url: "https://registry.example.com".to_string(),
            },
            digests: PackageDigests {
                artifact: artifact_digest.clone(),
                manifest: manifest_digest.clone(),
                content_root: None,
            },
            locked_at: 1,
        })
        .unwrap();
        lock.validate().unwrap();

        // Stage + activate.
        let mut env = Environment::new();
        let gen_id = env
            .stage(
                lock,
                BTreeMap::from([("xuepoo.theme".to_string(), vec!["fs.read".to_string()])]),
                100,
            )
            .unwrap();
        let act = activate(&mut env, gen_id, Some("0.6.0"), Some("1.0.0"), None).unwrap();
        assert!(act.succeeded);
        lifecycles
            .transition(&pid, PackageState::Activated)
            .unwrap();
        assert!(lifecycles.get(&pid).unwrap().state.may_execute_code());
        // Only activated may execute.
        assert!(!PackageState::Staged.may_execute_code());
    }

    #[test]
    fn tampered_artifact_blocks_before_staging() {
        let pid = PackageId::new("xuepoo.bad").unwrap();
        let manifest = PackageManifest {
            identity: PackageIdentity {
                id: pid.clone(),
                name: "Bad".to_string(),
                version: "0.1.0".to_string(),
                description: "desc".to_string(),
                license: None,
            },
            compat: Compat::default(),
            dependencies: Vec::new(),
            capabilities: Vec::new(),
            raw_bytes_len: 256,
            undeclared_fields: Vec::new(),
        };
        let artifact = b"good bytes";
        let bad_artifact = b"tampered";
        let good_digest = sha256_hex(artifact);
        let manifest_digest = manifest.canonical_digest();
        // Verify tampered bytes against good digest must fail at artifact stage.
        let inputs = VerificationInputs {
            artifact_bytes: bad_artifact,
            expected_artifact_digest: &good_digest,
            manifest: &manifest,
            expected_manifest_digest: &manifest_digest,
            granted_capabilities: &[],
            requested_capabilities: &[],
            capability_approval: false,
            host_bitty_version: Some("0.6.0"),
            host_plugin_api_version: Some("1.0.0"),
            expected_content_root: None,
            fetch_bytes: 10,
            fetch_elapsed_ms: 10,
            max_fetch_bytes: 1024,
            max_fetch_ms: 1000,
        };
        let report = verify_pipeline(&inputs);
        assert!(!report.is_passed());
        assert_eq!(
            report.first_failure().unwrap().stage,
            VerificationStage::ArtifactChecksum
        );
        // Tampered manifest semantics also independently detected via H-B.
        let mut tampered_manifest = manifest.clone();
        tampered_manifest
            .capabilities
            .push(CapabilityId::new("fs.write").unwrap());
        let inputs2 = VerificationInputs {
            artifact_bytes: artifact,
            expected_artifact_digest: &good_digest,
            manifest: &tampered_manifest,
            expected_manifest_digest: &manifest_digest,
            granted_capabilities: &[],
            requested_capabilities: &["fs.write".to_string()],
            capability_approval: false,
            host_bitty_version: Some("0.6.0"),
            host_plugin_api_version: Some("1.0.0"),
            expected_content_root: None,
            fetch_bytes: 10,
            fetch_elapsed_ms: 10,
            max_fetch_bytes: 1024,
            max_fetch_ms: 1000,
        };
        let report2 = verify_pipeline(&inputs2);
        assert!(!report2.is_passed());
        // Manifest hash binding fails (H-B), even though artifact itself is good.
        let has_hb_fail = report2
            .stages
            .iter()
            .any(|s| s.stage == VerificationStage::ManifestHashBinding && !s.passed);
        assert!(has_hb_fail);
    }

    #[test]
    fn local_path_drift_blocks_until_reresolve() {
        let files = vec![("a.txt", b"v1" as &[u8])];
        let digest = digest_local_content(&files);
        // Drift: file changed.
        let changed = vec![("a.txt", b"v2" as &[u8])];
        assert!(check_local_path_drift("xuepoo.local", &digest, &changed).is_err());
        // After re-resolve (new digest captured), no drift.
        let new_digest = digest_local_content(&changed);
        check_local_path_drift("xuepoo.local", &new_digest, &changed).unwrap();
    }

    #[test]
    fn trust_and_signature_flow() {
        let mut trust = TrustStore::new();
        let pid = PackageId::new("xuepoo.pkg").unwrap();
        trust
            .pin(TrustPin {
                package: pid.clone(),
                identity: "key-1".to_string(),
                mode: TrustMode::TrustOnFirstUse,
                first_seen: 1,
            })
            .unwrap();
        // Publisher key change is loud.
        assert!(trust.check(&pid, "key-2").is_err());

        // V-C signed verification.
        let mut keys = KeyStore::new();
        keys.insert(KeyRecord {
            key_id: "k1".to_string(),
            public_key_hex: "a".repeat(64),
            revoked: false,
        })
        .unwrap();
        let m = sha256_hex(b"m");
        let a = sha256_hex(b"a");
        let sig_hex = stub_sign("k1", &m, &a);
        let sig = SignatureRecord {
            key_id: "k1".to_string(),
            signature_hex: sig_hex,
            manifest_digest: m.clone(),
            artifact_digest: a.clone(),
        };
        verify_signature(&sig, &keys, &m, &a).unwrap();
        // Revoked key fails closed.
        keys.revoke("k1").unwrap();
        assert!(verify_signature(&sig, &keys, &m, &a).is_err());
    }

    #[test]
    fn rollback_determinism_and_capability_symmetry() {
        let mut env = Environment::new();
        let manifest = PackageManifest {
            identity: PackageIdentity {
                id: PackageId::new("xuepoo.cap").unwrap(),
                name: "Cap".to_string(),
                version: "0.1.0".to_string(),
                description: "desc".to_string(),
                license: None,
            },
            compat: Compat::default(),
            dependencies: Vec::new(),
            capabilities: vec![CapabilityId::new("fs.read").unwrap()],
            raw_bytes_len: 256,
            undeclared_fields: Vec::new(),
        };
        let m_digest = manifest.canonical_digest();
        let a_digest = sha256_hex(b"a");
        let mut lock1 = Lockfile::new();
        lock1
            .insert(LockedPackage {
                id: PackageId::new("xuepoo.cap").unwrap(),
                version: "0.1.0".to_string(),
                source: PackageSource::Registry {
                    url: "https://example.com".to_string(),
                },
                digests: PackageDigests {
                    artifact: a_digest.clone(),
                    manifest: m_digest.clone(),
                    content_root: None,
                },
                locked_at: 1,
            })
            .unwrap();
        let digest1 = lock1.digest();
        let id1 = env
            .stage(
                lock1,
                BTreeMap::from([("xuepoo.cap".to_string(), vec!["fs.read".to_string()])]),
                1,
            )
            .unwrap();
        activate(&mut env, id1, Some("0.6.0"), Some("1.0.0"), None).unwrap();

        // Update to broader capability — approval required to stage second gen.
        let granted = vec!["fs.read".to_string()];
        let requested = vec!["fs.read".to_string(), "fs.write".to_string()];
        // Without approval, diff blocks.
        assert!(check_capability_diff(&granted, &requested, false).is_err());
        // With approval, proceed.
        check_capability_diff(&granted, &requested, true).unwrap();
        let mut lock2 = Lockfile::new();
        lock2
            .insert(LockedPackage {
                id: PackageId::new("xuepoo.cap").unwrap(),
                version: "0.2.0".to_string(),
                source: PackageSource::Registry {
                    url: "https://example.com".to_string(),
                },
                digests: PackageDigests {
                    artifact: sha256_hex(b"a2"),
                    manifest: sha256_hex(b"m2"),
                    content_root: None,
                },
                locked_at: 2,
            })
            .unwrap();
        let id2 = env
            .stage(
                lock2,
                BTreeMap::from([(
                    "xuepoo.cap".to_string(),
                    vec!["fs.read".to_string(), "fs.write".to_string()],
                )]),
                2,
            )
            .unwrap();
        activate(&mut env, id2, Some("0.6.0"), Some("1.0.0"), None).unwrap();
        assert_eq!(env.current, Some(id2));

        // Full rollback to id1 restores exactly.
        rollback_full(&mut env, id1, None).unwrap();
        assert_eq!(env.current, Some(id1));
        assert_eq!(env.current_generation().unwrap().lock.digest(), digest1);

        // Redo to higher capability again still requires approval (symmetric gate).
        assert!(check_capability_diff(&granted, &requested, false).is_err());
    }
}
