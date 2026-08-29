#![forbid(unsafe_code)]

//! Hostile package tests — Phase E deep (CTX-0080).
//!
//! Bounded, headless, deterministic, `forbid(unsafe)`.
//! Covers: cycles/diamond, yanked, prerelease, corrupt lock/package,
//! signature mismatch, symlink/path traversal, concurrent install/determinism,
//! disk-full/budget, capability escalation, key rotation/revoked stale snapshot.

use bitty_package::{
    CapabilityId, Compat, LockedPackage, Lockfile, PackageDependency, PackageDigests, PackageId,
    PackageIdentity, PackageManifest, PackageSource, Version, VersionReq, sha256_hex,
};
use bitty_package::{IndexEntry, PackageIndex, resolve, resolve_preserving_locked};
use bitty_package::{
    KeyRecord, KeyStore, SignatureRecord, TrustMode, TrustPin, TrustStore, stub_sign,
    verify_signature,
};
use bitty_package::{
    MANIFEST_MAX_BYTES, MAX_ARTIFACT_BYTES, VerificationInputs, VerificationStage,
    check_fetch_framing, verify_artifact_checksum, verify_pipeline,
};
use bitty_package::{
    check_capability_diff, check_local_path_drift, digest_local_content,
    ensure_no_promotion_without_chain,
};

// ── helpers ────────────────────────────────────────────────────────────────

fn pid(s: &str) -> PackageId {
    PackageId::new(s).unwrap()
}

fn dep(id: &str, req: &str) -> PackageDependency {
    PackageDependency {
        id: pid(id),
        version_req: req.to_string(),
        prerelease: false,
    }
}

fn dep_pre(id: &str, req: &str, pre: bool) -> PackageDependency {
    PackageDependency {
        id: pid(id),
        version_req: req.to_string(),
        prerelease: pre,
    }
}

fn manifest_with_deps(deps: Vec<PackageDependency>) -> PackageManifest {
    PackageManifest {
        identity: PackageIdentity {
            id: pid("xuepoo.root"),
            name: "Root".to_string(),
            version: "0.1.0".to_string(),
            description: "root".to_string(),
            license: None,
        },
        compat: Compat::default(),
        dependencies: deps,
        capabilities: Vec::new(),
        raw_bytes_len: 256,
        undeclared_fields: Vec::new(),
    }
}

fn entry(id: &str, ver: &str, yanked: bool, deps: Vec<PackageDependency>) -> IndexEntry {
    IndexEntry::new(pid(id), ver.to_string(), yanked, deps).unwrap()
}

fn minimal_manifest(id: &str) -> PackageManifest {
    PackageManifest {
        identity: PackageIdentity {
            id: pid(id),
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

// ── cycles / diamond ─────────────────────────────────────────────────────

#[test]
fn cycle_converges_when_consistent() {
    let mut idx = PackageIndex::new();
    idx.insert(entry(
        "xuepoo.a",
        "1.0.0",
        false,
        vec![dep("xuepoo.b", "^1.0")],
    ))
    .unwrap();
    idx.insert(entry(
        "xuepoo.b",
        "1.0.0",
        false,
        vec![dep("xuepoo.a", "^1.0")],
    ))
    .unwrap();
    let m = manifest_with_deps(vec![dep("xuepoo.a", "^1.0")]);
    let res = resolve(&m, &idx).unwrap();
    assert!(res.packages.contains_key(&pid("xuepoo.a")));
    assert!(res.packages.contains_key(&pid("xuepoo.b")));
}

#[test]
fn cycle_conflict_reports_named_edges() {
    let mut idx = PackageIndex::new();
    idx.insert(entry(
        "xuepoo.a",
        "1.0.0",
        false,
        vec![dep("xuepoo.b", "^1.0")],
    ))
    .unwrap();
    idx.insert(entry(
        "xuepoo.a",
        "2.0.0",
        false,
        vec![dep("xuepoo.b", "^2.0")],
    ))
    .unwrap();
    idx.insert(entry(
        "xuepoo.b",
        "1.0.0",
        false,
        vec![dep("xuepoo.a", "^1.0")],
    ))
    .unwrap();
    idx.insert(entry(
        "xuepoo.b",
        "2.0.0",
        false,
        vec![dep("xuepoo.a", "^2.0")],
    ))
    .unwrap();
    // Root forces a 1.0 while transitive via b would need 2.0 — need conflict
    let m = manifest_with_deps(vec![dep("xuepoo.a", "=1.0.0"), dep("xuepoo.b", "=2.0.0")]);
    let err = resolve(&m, &idx).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("resolver"), "msg: {msg}");
}

#[test]
fn diamond_converges_when_compatible() {
    let mut idx = PackageIndex::new();
    idx.insert(entry(
        "xuepoo.a",
        "1.0.0",
        false,
        vec![dep("xuepoo.dep", "^1.0")],
    ))
    .unwrap();
    idx.insert(entry(
        "xuepoo.b",
        "1.0.0",
        false,
        vec![dep("xuepoo.dep", "^1.0")],
    ))
    .unwrap();
    idx.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
        .unwrap();
    idx.insert(entry("xuepoo.dep", "1.5.0", false, vec![]))
        .unwrap();
    let m = manifest_with_deps(vec![dep("xuepoo.a", "^1.0"), dep("xuepoo.b", "^1.0")]);
    let res = resolve(&m, &idx).unwrap();
    assert_eq!(res.packages[&pid("xuepoo.dep")].version, "1.5.0");
}

#[test]
fn diamond_conflict_names_both_edges() {
    let mut idx = PackageIndex::new();
    idx.insert(entry(
        "xuepoo.a",
        "1.0.0",
        false,
        vec![dep("xuepoo.dep", "^1.0")],
    ))
    .unwrap();
    idx.insert(entry(
        "xuepoo.b",
        "1.0.0",
        false,
        vec![dep("xuepoo.dep", "^2.0")],
    ))
    .unwrap();
    idx.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
        .unwrap();
    idx.insert(entry("xuepoo.dep", "2.0.0", false, vec![]))
        .unwrap();
    let m = manifest_with_deps(vec![dep("xuepoo.a", "^1.0"), dep("xuepoo.b", "^1.0")]);
    let err = resolve(&m, &idx).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("xuepoo.dep"), "msg must name dep: {msg}");
    assert!(msg.contains("requires"), "msg must name edges: {msg}");
}

#[test]
fn self_dependency_rejected_at_manifest_validate() {
    let mut m = minimal_manifest("xuepoo.selfdep");
    m.dependencies.push(dep("xuepoo.selfdep", "^1.0"));
    assert!(m.validate().is_err());
    let msg = format!("{}", m.validate().unwrap_err());
    assert!(msg.contains("must not depend on itself"));
}

// ── yanked ─────────────────────────────────────────────────────────────────

#[test]
fn yanked_max_filtered_picks_next_stable() {
    let mut idx = PackageIndex::new();
    idx.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
        .unwrap();
    idx.insert(entry("xuepoo.dep", "1.2.0", false, vec![]))
        .unwrap();
    idx.insert(entry("xuepoo.dep", "1.5.0", true, vec![]))
        .unwrap();
    let m = manifest_with_deps(vec![dep("xuepoo.dep", ">=1.0.0")]);
    let res = resolve(&m, &idx).unwrap();
    assert_eq!(res.packages[&pid("xuepoo.dep")].version, "1.2.0");
}

#[test]
fn yanked_no_alternative_reports_conflict() {
    let mut idx = PackageIndex::new();
    idx.insert(entry("xuepoo.dep", "2.0.0", true, vec![]))
        .unwrap();
    let m = manifest_with_deps(vec![dep("xuepoo.dep", "^2.0")]);
    let err = resolve(&m, &idx).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("xuepoo.dep"), "must name package: {msg}");
}

#[test]
fn yanked_transitive_skipped() {
    let mut idx = PackageIndex::new();
    idx.insert(entry(
        "xuepoo.a",
        "1.0.0",
        false,
        vec![dep("xuepoo.dep", "^1.0")],
    ))
    .unwrap();
    idx.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
        .unwrap();
    idx.insert(entry("xuepoo.dep", "2.0.0", true, vec![]))
        .unwrap();
    let m = manifest_with_deps(vec![dep("xuepoo.a", "^1.0")]);
    let res = resolve(&m, &idx).unwrap();
    assert_eq!(res.packages[&pid("xuepoo.dep")].version, "1.0.0");
}

#[test]
fn yanked_preserved_locked_warns_and_picks_yanked() {
    let mut idx = PackageIndex::new();
    idx.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
        .unwrap();
    idx.insert(entry("xuepoo.dep", "2.0.0", true, vec![]))
        .unwrap();
    let m = manifest_with_deps(vec![dep("xuepoo.dep", ">=1.0.0")]);
    let mut lock = Lockfile::new();
    lock.insert(LockedPackage {
        id: pid("xuepoo.dep"),
        version: "2.0.0".to_string(),
        source: PackageSource::Registry {
            url: "https://example.com".to_string(),
        },
        digests: PackageDigests {
            artifact: sha256_hex(b"a"),
            manifest: sha256_hex(b"m"),
            content_root: None,
        },
        locked_at: 1,
    })
    .unwrap();
    let (preserved, warnings) = resolve_preserving_locked(&m, &idx, &lock).unwrap();
    assert_eq!(preserved.packages[&pid("xuepoo.dep")].version, "2.0.0");
    assert!(warnings.iter().any(|w| w.contains("yanked (locked)")));
}

// ── prerelease ───────────────────────────────────────────────────────────────

#[test]
fn prerelease_excluded_without_opt_in() {
    let mut idx = PackageIndex::new();
    idx.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
        .unwrap();
    idx.insert(entry("xuepoo.dep", "1.1.0-alpha.1", false, vec![]))
        .unwrap();
    let m = manifest_with_deps(vec![dep("xuepoo.dep", "^1.0")]);
    let res = resolve(&m, &idx).unwrap();
    assert_eq!(res.packages[&pid("xuepoo.dep")].version, "1.0.0");
}

#[test]
fn prerelease_opt_in_via_flag() {
    let mut idx = PackageIndex::new();
    idx.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
        .unwrap();
    idx.insert(entry("xuepoo.dep", "1.1.0-alpha.1", false, vec![]))
        .unwrap();
    let m = manifest_with_deps(vec![dep_pre("xuepoo.dep", "^1.0", true)]);
    let res = resolve(&m, &idx).unwrap();
    assert_eq!(res.packages[&pid("xuepoo.dep")].version, "1.1.0-alpha.1");
}

#[test]
fn prerelease_opt_in_via_same_core_comparator() {
    let mut idx = PackageIndex::new();
    idx.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
        .unwrap();
    idx.insert(entry("xuepoo.dep", "1.1.0-alpha.1", false, vec![]))
        .unwrap();
    idx.insert(entry("xuepoo.dep", "1.1.0-alpha.2", false, vec![]))
        .unwrap();
    let m = manifest_with_deps(vec![dep("xuepoo.dep", ">=1.1.0-alpha")]);
    let res = resolve(&m, &idx).unwrap();
    assert_eq!(res.packages[&pid("xuepoo.dep")].version, "1.1.0-alpha.2");
}

#[test]
fn prerelease_mixed_edges_blocks() {
    let mut idx = PackageIndex::new();
    idx.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
        .unwrap();
    idx.insert(entry("xuepoo.dep", "1.1.0-alpha.1", false, vec![]))
        .unwrap();
    idx.insert(entry(
        "xuepoo.a",
        "1.0.0",
        false,
        vec![dep_pre("xuepoo.dep", "^1.0", true)],
    ))
    .unwrap();
    idx.insert(entry(
        "xuepoo.b",
        "1.0.0",
        false,
        vec![dep("xuepoo.dep", "^1.0")],
    ))
    .unwrap();
    let m = manifest_with_deps(vec![dep("xuepoo.a", "^1.0"), dep("xuepoo.b", "^1.0")]);
    let res = resolve(&m, &idx).unwrap();
    // Mixed: a opts in, b does not — prerelease must be blocked, so stable 1.0.0
    assert_eq!(res.packages[&pid("xuepoo.dep")].version, "1.0.0");
}

// ── corrupt lock / package ─────────────────────────────────────────────────

#[test]
fn corrupt_lock_invalid_hex_rejected() {
    let mut p = LockedPackage {
        id: pid("xuepoo.a"),
        version: "0.1.0".to_string(),
        source: PackageSource::Registry {
            url: "https://example.com".to_string(),
        },
        digests: PackageDigests {
            artifact: "not-hex".to_string(),
            manifest: sha256_hex(b"m"),
            content_root: None,
        },
        locked_at: 1,
    };
    assert!(p.validate().is_err());
    p.digests.artifact = sha256_hex(b"a");
    p.digests.manifest = "short".to_string();
    assert!(p.validate().is_err());
    p.digests.manifest = sha256_hex(b"m");
    p.digests.content_root = Some("bad".to_string());
    assert!(p.validate().is_err());
}

#[test]
fn corrupt_lock_duplicate_id_rejected() {
    let mut lf = Lockfile::new();
    lf.insert(LockedPackage {
        id: pid("xuepoo.a"),
        version: "0.1.0".to_string(),
        source: PackageSource::Registry {
            url: "https://example.com".to_string(),
        },
        digests: PackageDigests {
            artifact: sha256_hex(b"a"),
            manifest: sha256_hex(b"m"),
            content_root: None,
        },
        locked_at: 1,
    })
    .unwrap();
    assert!(
        lf.insert(LockedPackage {
            id: pid("xuepoo.a"),
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
        .is_err()
    );
}

#[test]
fn corrupt_lock_wrong_version_rejected() {
    let mut lf = Lockfile::new();
    let bad = LockedPackage {
        id: pid("xuepoo.a"),
        version: "not-semver".to_string(),
        source: PackageSource::Registry {
            url: "https://example.com".to_string(),
        },
        digests: PackageDigests {
            artifact: sha256_hex(b"a"),
            manifest: sha256_hex(b"m"),
            content_root: None,
        },
        locked_at: 1,
    };
    assert!(lf.insert(bad).is_err());
}

#[test]
fn corrupt_lock_unsupported_version_rejected() {
    let mut lf = Lockfile::new();
    lf.version = 999;
    assert!(lf.validate().is_err());
}

#[test]
fn corrupt_lock_oversize_rejected() {
    let mut lf = Lockfile::new();
    for i in 0..1025 {
        let id = format!("xuepoo.p{i:04}");
        lf.insert(LockedPackage {
            id: pid(&id),
            version: "0.1.0".to_string(),
            source: PackageSource::Registry {
                url: "https://example.com".to_string(),
            },
            digests: PackageDigests {
                artifact: sha256_hex(b"a"),
                manifest: sha256_hex(b"m"),
                content_root: None,
            },
            locked_at: i as u64,
        })
        .unwrap();
    }
    assert!(lf.validate().is_err());
}

#[test]
fn corrupt_lock_tampered_artifact_detected() {
    let m = minimal_manifest("xuepoo.pkg");
    let artifact = b"good bytes";
    let good_digest = sha256_hex(artifact);
    let bad = b"tampered";
    let m_digest = m.canonical_digest();
    let inputs = VerificationInputs {
        artifact_bytes: bad,
        expected_artifact_digest: &good_digest,
        manifest: &m,
        expected_manifest_digest: &m_digest,
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
}

#[test]
fn corrupt_package_unknown_field_rejected() {
    let mut m = minimal_manifest("xuepoo.pkg");
    m.undeclared_fields.push("evil".to_string());
    assert!(m.validate().is_err());
}

#[test]
fn corrupt_package_bytes_over_limit_rejected() {
    let mut m = minimal_manifest("xuepoo.pkg");
    m.raw_bytes_len = MANIFEST_MAX_BYTES + 1;
    assert!(m.validate().is_err());
}

#[test]
fn corrupt_package_invalid_id_rejected() {
    assert!(PackageId::new("INVALID").is_err());
    assert!(PackageId::new("owner.").is_err());
    assert!(PackageId::new(".name").is_err());
    assert!(PackageId::new("a.b.c").is_err());
}

#[test]
fn corrupt_manifest_duplicate_dep_rejected() {
    let mut m = minimal_manifest("xuepoo.pkg");
    m.dependencies.push(dep("xuepoo.dep", "^1.0"));
    m.dependencies.push(dep("xuepoo.dep", "^2.0"));
    assert!(m.validate().is_err());
}

#[test]
fn corrupt_manifest_duplicate_cap_rejected() {
    let mut m = minimal_manifest("xuepoo.pkg");
    m.capabilities.push(CapabilityId::new("fs.read").unwrap());
    m.capabilities.push(CapabilityId::new("fs.read").unwrap());
    assert!(m.validate().is_err());
}

#[test]
fn corrupt_package_cap_invalid_family_rejected() {
    assert!(CapabilityId::new("evil.read").is_err());
    // Wildcard in head is rejected; param wildcard like fs.read:/tmp/* is allowed (param), head wildcard is not.
    assert!(CapabilityId::new("fs.*").is_err());
    assert!(CapabilityId::new("fs.read:").is_err());
}

// ── signature mismatch ─────────────────────────────────────────────────────

#[test]
fn signature_mismatch_unknown_key_rejected() {
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
    let bad = SignatureRecord {
        key_id: "unknown".to_string(),
        signature_hex: sig_hex,
        manifest_digest: m.clone(),
        artifact_digest: a.clone(),
    };
    assert!(verify_signature(&bad, &keys, &m, &a).is_err());
}

#[test]
fn signature_mismatch_revoked_key_rejected() {
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
    keys.revoke("k1").unwrap();
    let sig = SignatureRecord {
        key_id: "k1".to_string(),
        signature_hex: sig_hex,
        manifest_digest: m.clone(),
        artifact_digest: a.clone(),
    };
    assert!(verify_signature(&sig, &keys, &m, &a).is_err());
}

#[test]
fn signature_mismatch_over_different_bytes_rejected() {
    let mut keys = KeyStore::new();
    keys.insert(KeyRecord {
        key_id: "k1".to_string(),
        public_key_hex: "a".repeat(64),
        revoked: false,
    })
    .unwrap();
    let m = sha256_hex(b"m");
    let a = sha256_hex(b"a");
    let m2 = sha256_hex(b"other");
    let sig_hex = stub_sign("k1", &m, &a);
    let sig = SignatureRecord {
        key_id: "k1".to_string(),
        signature_hex: sig_hex,
        manifest_digest: m2.clone(),
        artifact_digest: a.clone(),
    };
    assert!(verify_signature(&sig, &keys, &m, &a).is_err());
}

#[test]
fn signature_mismatch_bad_hex_len_rejected() {
    let mut keys = KeyStore::new();
    keys.insert(KeyRecord {
        key_id: "k1".to_string(),
        public_key_hex: "a".repeat(64),
        revoked: false,
    })
    .unwrap();
    let m = sha256_hex(b"m");
    let a = sha256_hex(b"a");
    let sig = SignatureRecord {
        key_id: "k1".to_string(),
        signature_hex: "bad".to_string(),
        manifest_digest: m.clone(),
        artifact_digest: a.clone(),
    };
    assert!(verify_signature(&sig, &keys, &m, &a).is_err());
}

#[test]
fn signature_valid_then_revoked_fails_stale_snapshot() {
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
        signature_hex: sig_hex.clone(),
        manifest_digest: m.clone(),
        artifact_digest: a.clone(),
    };
    verify_signature(&sig, &keys, &m, &a).unwrap();
    keys.revoke("k1").unwrap();
    assert!(verify_signature(&sig, &keys, &m, &a).is_err());
}

// ── key rotation / stale snapshot ──────────────────────────────────────────

#[test]
fn publisher_key_rotation_new_key_valid_old_revoked_stale_fails() {
    let mut keys = KeyStore::new();
    keys.insert(KeyRecord {
        key_id: "k1".to_string(),
        public_key_hex: "a".repeat(64),
        revoked: false,
    })
    .unwrap();
    keys.insert(KeyRecord {
        key_id: "k2".to_string(),
        public_key_hex: "b".repeat(64),
        revoked: false,
    })
    .unwrap();
    let m = sha256_hex(b"m");
    let a = sha256_hex(b"a");
    let sig_k2 = SignatureRecord {
        key_id: "k2".to_string(),
        signature_hex: stub_sign("k2", &m, &a),
        manifest_digest: m.clone(),
        artifact_digest: a.clone(),
    };
    verify_signature(&sig_k2, &keys, &m, &a).unwrap();
    // Rotate: revoke k1
    keys.revoke("k1").unwrap();
    let sig_k1 = SignatureRecord {
        key_id: "k1".to_string(),
        signature_hex: stub_sign("k1", &m, &a),
        manifest_digest: m.clone(),
        artifact_digest: a.clone(),
    };
    assert!(verify_signature(&sig_k1, &keys, &m, &a).is_err());
    // k2 still valid
    verify_signature(&sig_k2, &keys, &m, &a).unwrap();
}

#[test]
fn tofu_pin_change_loud_and_reapprove() {
    let mut store = TrustStore::new();
    store
        .pin(TrustPin {
            package: pid("xuepoo.a"),
            identity: "key-old".to_string(),
            mode: TrustMode::TrustOnFirstUse,
            first_seen: 1,
        })
        .unwrap();
    assert!(store.check(&pid("xuepoo.a"), "key-new").is_err());
    store
        .reapprove(pid("xuepoo.a"), "key-new".to_string(), 2)
        .unwrap();
    assert!(store.check(&pid("xuepoo.a"), "key-new").is_ok());
}

// ── symlink / path traversal ───────────────────────────────────────────────

#[test]
fn local_path_traversal_not_registry_provenance() {
    let s = PackageSource::LocalPath {
        path: "../etc/passwd".to_string(),
        content_digest: sha256_hex(b"x"),
    };
    s.validate().unwrap();
    assert!(!s.has_registry_provenance());
    assert!(ensure_no_promotion_without_chain(&s, true).is_err());
    assert!(ensure_no_promotion_without_chain(&s, false).is_ok());
}

#[test]
fn local_path_nul_rejected() {
    let s = PackageSource::LocalPath {
        path: "a\0b".to_string(),
        content_digest: sha256_hex(b"x"),
    };
    assert!(s.validate().is_err());
}

#[test]
fn local_path_empty_rejected() {
    let s = PackageSource::LocalPath {
        path: "".to_string(),
        content_digest: sha256_hex(b"x"),
    };
    assert!(s.validate().is_err());
}

#[test]
fn symlink_like_path_digest_determinism() {
    let files_a = vec![
        ("a/../b.txt", b"hello" as &[u8]),
        ("b.txt", b"world" as &[u8]),
    ];
    let files_b = vec![
        ("b.txt", b"world" as &[u8]),
        ("a/../b.txt", b"hello" as &[u8]),
    ];
    assert_eq!(
        digest_local_content(&files_a),
        digest_local_content(&files_b)
    );
    let d = digest_local_content(&files_a);
    check_local_path_drift("xuepoo.pkg", &d, &files_a).unwrap();
    let changed = vec![
        ("a/../b.txt", b"changed" as &[u8]),
        ("b.txt", b"world" as &[u8]),
    ];
    assert!(check_local_path_drift("xuepoo.pkg", &d, &changed).is_err());
}

// ── concurrent install / determinism ───────────────────────────────────────

#[test]
fn concurrent_install_deterministic_digest() {
    let mut idx = PackageIndex::new();
    for v in ["1.0.0", "1.1.0", "1.2.0"] {
        idx.insert(entry("xuepoo.dep", v, false, vec![])).unwrap();
    }
    let m = manifest_with_deps(vec![dep("xuepoo.dep", ">=1.0.0, <1.3.0")]);
    let r1 = resolve(&m, &idx).unwrap();
    let r2 = resolve(&m, &idx).unwrap();
    assert_eq!(r1.digest(), r2.digest());
    let mut lock1 = Lockfile::new();
    lock1
        .insert(LockedPackage {
            id: pid("xuepoo.dep"),
            version: "1.2.0".to_string(),
            source: PackageSource::Registry {
                url: "https://example.com".to_string(),
            },
            digests: PackageDigests {
                artifact: sha256_hex(b"a"),
                manifest: sha256_hex(b"m"),
                content_root: None,
            },
            locked_at: 1,
        })
        .unwrap();
    let mut lock2 = lock1.clone();
    assert_eq!(lock1.digest(), lock2.digest());
    // Lock digest determinism regardless of insertion order
    let mut lf1 = Lockfile::new();
    lf1.insert(LockedPackage {
        id: pid("xuepoo.b"),
        version: "0.1.0".to_string(),
        source: PackageSource::Registry {
            url: "https://example.com".to_string(),
        },
        digests: PackageDigests {
            artifact: sha256_hex(b"b"),
            manifest: sha256_hex(b"mb"),
            content_root: None,
        },
        locked_at: 1,
    })
    .unwrap();
    lf1.insert(LockedPackage {
        id: pid("xuepoo.a"),
        version: "0.1.0".to_string(),
        source: PackageSource::Registry {
            url: "https://example.com".to_string(),
        },
        digests: PackageDigests {
            artifact: sha256_hex(b"a"),
            manifest: sha256_hex(b"ma"),
            content_root: None,
        },
        locked_at: 2,
    })
    .unwrap();
    let mut lf2 = Lockfile::new();
    lf2.insert(LockedPackage {
        id: pid("xuepoo.a"),
        version: "0.1.0".to_string(),
        source: PackageSource::Registry {
            url: "https://example.com".to_string(),
        },
        digests: PackageDigests {
            artifact: sha256_hex(b"a"),
            manifest: sha256_hex(b"ma"),
            content_root: None,
        },
        locked_at: 2,
    })
    .unwrap();
    lf2.insert(LockedPackage {
        id: pid("xuepoo.b"),
        version: "0.1.0".to_string(),
        source: PackageSource::Registry {
            url: "https://example.com".to_string(),
        },
        digests: PackageDigests {
            artifact: sha256_hex(b"b"),
            manifest: sha256_hex(b"mb"),
            content_root: None,
        },
        locked_at: 1,
    })
    .unwrap();
    assert_eq!(lf1.digest(), lf2.digest());
    // Also test that concurrent-like sequential lock operations produce same digest
    lock2.packages.reverse();
    assert_eq!(lock1.digest(), lock2.digest());
}

#[test]
fn concurrent_install_comma_order_invariant() {
    let mut idx = PackageIndex::new();
    for v in ["1.0.0", "1.1.0", "1.5.0"] {
        idx.insert(entry("xuepoo.dep", v, false, vec![])).unwrap();
    }
    let m1 = manifest_with_deps(vec![PackageDependency {
        id: pid("xuepoo.dep"),
        version_req: ">=1.0.0, <1.3.0".to_string(),
        prerelease: false,
    }]);
    let m2 = manifest_with_deps(vec![PackageDependency {
        id: pid("xuepoo.dep"),
        version_req: "<1.3.0, >=1.0.0".to_string(),
        prerelease: false,
    }]);
    let r1 = resolve(&m1, &idx).unwrap();
    let r2 = resolve(&m2, &idx).unwrap();
    assert_eq!(r1.digest(), r2.digest());
}

// ── disk full / budget ─────────────────────────────────────────────────────

#[test]
fn disk_full_manifest_over_limit_rejected() {
    let mut m = minimal_manifest("xuepoo.big");
    m.raw_bytes_len = MANIFEST_MAX_BYTES + 1;
    assert!(m.validate().is_err());
    let err = m.validate().unwrap_err();
    assert!(format!("{err}").contains("limit") || format!("{err}").contains("exceeds"));
}

#[test]
fn disk_full_fetch_framing_bytes_over_budget() {
    assert!(check_fetch_framing(MAX_ARTIFACT_BYTES + 1, 10, MAX_ARTIFACT_BYTES, 5000).is_err());
    assert!(verify_artifact_checksum(b"hi", &"a".repeat(64)).is_err());
}

#[test]
fn disk_full_fetch_time_over_budget() {
    assert!(check_fetch_framing(100, 6000, MAX_ARTIFACT_BYTES, 5000).is_err());
}

#[test]
fn disk_full_lock_packages_over_limit() {
    let mut lf = Lockfile::new();
    for i in 0..1025 {
        lf.insert(LockedPackage {
            id: pid(&format!("xuepoo.overflow{i:04}")),
            version: "0.1.0".to_string(),
            source: PackageSource::Registry {
                url: "https://example.com".to_string(),
            },
            digests: PackageDigests {
                artifact: sha256_hex(b"a"),
                manifest: sha256_hex(b"m"),
                content_root: None,
            },
            locked_at: i as u64,
        })
        .unwrap();
    }
    assert!(lf.validate().is_err());
}

#[test]
fn disk_full_resolver_budget_exceeded() {
    let mut idx = PackageIndex::new();
    idx.insert(entry("xuepoo.dep", "1.0.0", false, vec![]))
        .unwrap();
    let many: Vec<PackageDependency> = (0..65)
        .map(|i| dep(&format!("xuepoo.p{i:02}"), "^1.0"))
        .collect();
    let m = manifest_with_deps(many);
    let err = resolve(&m, &idx).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("exceeds") || msg.contains("budget") || msg.contains("limit"));
}

// ── capability escalation ──────────────────────────────────────────────────

#[test]
fn capability_escalation_tampered_manifest_blocks_via_hb() {
    let m = minimal_manifest("xuepoo.cap");
    let good_m_digest = m.canonical_digest();
    let mut tampered = m.clone();
    tampered
        .capabilities
        .push(CapabilityId::new("fs.write").unwrap());
    assert_ne!(tampered.canonical_digest(), good_m_digest);
    let artifact = b"pkg";
    let a_digest = sha256_hex(artifact);
    let inputs = VerificationInputs {
        artifact_bytes: artifact,
        expected_artifact_digest: &a_digest,
        manifest: &tampered,
        expected_manifest_digest: &good_m_digest,
        granted_capabilities: &[],
        requested_capabilities: &["fs.write".to_string()],
        capability_approval: false,
        host_bitty_version: Some("0.6.0"),
        host_plugin_api_version: Some("1.0.0"),
        expected_content_root: None,
        fetch_bytes: artifact.len(),
        fetch_elapsed_ms: 10,
        max_fetch_bytes: MAX_ARTIFACT_BYTES,
        max_fetch_ms: 5000,
    };
    let report = verify_pipeline(&inputs);
    assert!(!report.is_passed());
    let hb_fail = report
        .stages
        .iter()
        .any(|s| s.stage == VerificationStage::ManifestHashBinding && !s.passed);
    assert!(hb_fail);
}

#[test]
fn capability_escalation_diff_blocks_without_approval() {
    let granted = vec!["fs.read".to_string()];
    let requested = vec!["fs.read".to_string(), "fs.write".to_string()];
    assert!(check_capability_diff(&granted, &requested, false).is_err());
    assert!(check_capability_diff(&granted, &requested, true).is_ok());
}

#[test]
fn capability_escalation_narrowing_allowed() {
    let granted = vec!["fs.read".to_string(), "fs.write".to_string()];
    let narrowed = vec!["fs.read".to_string()];
    assert!(check_capability_diff(&granted, &narrowed, false).is_ok());
}

#[test]
fn capability_escalation_via_verify_pipeline_stage5() {
    let m = minimal_manifest("xuepoo.cap2");
    let m_digest = m.canonical_digest();
    let artifact = b"bytes";
    let a_digest = sha256_hex(artifact);
    let granted = vec!["fs.read".to_string()];
    let requested = vec!["fs.read".to_string(), "network.connect".to_string()];
    let inputs = VerificationInputs {
        artifact_bytes: artifact,
        expected_artifact_digest: &a_digest,
        manifest: &m,
        expected_manifest_digest: &m_digest,
        granted_capabilities: &granted,
        requested_capabilities: &requested,
        capability_approval: false,
        host_bitty_version: Some("0.6.0"),
        host_plugin_api_version: Some("1.0.0"),
        expected_content_root: None,
        fetch_bytes: artifact.len(),
        fetch_elapsed_ms: 10,
        max_fetch_bytes: MAX_ARTIFACT_BYTES,
        max_fetch_ms: 5000,
    };
    let report = verify_pipeline(&inputs);
    assert!(!report.is_passed());
    assert_eq!(
        report.first_failure().unwrap().stage,
        VerificationStage::CapabilityDiff
    );
}

// ── requirement / version hostile ──────────────────────────────────────────

#[test]
fn hostile_requirement_wildcard_and_disjunction_rejected() {
    assert!(VersionReq::parse("*").is_err());
    assert!(VersionReq::parse(">=1.0 || <2.0").is_err());
    assert!(VersionReq::parse("^1.0, <2.0").is_err());
}

#[test]
fn hostile_version_invalid_char_and_leading_zero_rejected() {
    assert!(Version::parse("01.0.0").is_err());
    assert!(Version::parse("1.0.0*").is_err());
    assert!(Version::parse("1.0.0-alpha.01").is_err());
}

#[test]
fn hostile_requirement_empty_comparator_rejected() {
    assert!(VersionReq::parse(">=1.0,,<2.0").is_err());
    assert!(VersionReq::parse("").is_err());
}
