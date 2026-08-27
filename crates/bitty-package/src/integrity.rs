//! Integrity verification chain — 7 stages per RFC.
//!
//! Each stage consumes the output of the previous one and none may be
//! skipped for any source type. Bundled and local-path sources use degenerate
//! records rather than exemptions.

use crate::error::PackageError;
use crate::manifest::PackageManifest;

// ── digest helpers ───────────────────────────────────────────────────────

// Digest is SHA-256 hex (64 lower-hex chars).
const DIGEST_HEX_LEN: usize = 64;

/// Whether `s` is a valid lower/upper hex digest of length 64.
#[must_use]
pub fn is_valid_hex_digest(s: &str) -> bool {
    s.len() == DIGEST_HEX_LEN && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Compute SHA-256 hex over `bytes` (pure Rust, no external crate, no unsafe).
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let hash = sha256(bytes);
    let mut s = String::with_capacity(64);
    for b in hash {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Verify that `s` is valid hex digest format; else err.
pub fn validate_hex_digest(s: &str, field: &str) -> Result<(), PackageError> {
    if !is_valid_hex_digest(s) {
        return Err(PackageError::manifest(
            field,
            format!("digest '{s}' must be 64 hex chars (SHA-256)"),
        ));
    }
    Ok(())
}

// Minimal SHA-256 implementation (FIPS 180-4). No unsafe, no alloc beyond output.
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    // Initial hash values.
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    // Pre-processing: padding.
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    // Process each 512-bit chunk.
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, val) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&val.to_be_bytes());
    }
    out
}

// ── limits for fetch framing (Invariant 7) ───────────────────────────────

/// Maximum artifact bytes (10 MiB candidate).
pub const MAX_ARTIFACT_BYTES: usize = 10 * 1024 * 1024;
/// Maximum manifest bytes (re-export for framing checks).
pub use crate::manifest::MANIFEST_MAX_BYTES;

// ── verification stage enum ──────────────────────────────────────────────

/// Ordered verification pipeline — each stage consumes previous output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VerificationStage {
    /// 1. Transfer-size and time budgets before/during download.
    FetchFraming,
    /// 2. Artifact digest vs lock record (H-A).
    ArtifactChecksum,
    /// 3. Schema, field, and limit validation of manifest.
    ManifestValidation,
    /// 4. Canonical manifest digest vs lock (H-B).
    ManifestHashBinding,
    /// 5. Capability diff against grant set (P0-AC-030).
    CapabilityDiff,
    /// 6. Plugin API and Bitty compatibility.
    CompatibilityCheck,
    /// 7. Verified content written into package store under digests.
    StoreCommit,
}

impl VerificationStage {
    /// Human label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::FetchFraming => "fetch_framing",
            Self::ArtifactChecksum => "artifact_checksum",
            Self::ManifestValidation => "manifest_validation",
            Self::ManifestHashBinding => "manifest_hash_binding",
            Self::CapabilityDiff => "capability_diff",
            Self::CompatibilityCheck => "compatibility_check",
            Self::StoreCommit => "store_commit",
        }
    }
}

impl std::fmt::Display for VerificationStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

// ── per-stage results + overall report ───────────────────────────────────

/// Result of a single verification stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageResult {
    /// Stage.
    pub stage: VerificationStage,
    /// Whether it passed.
    pub passed: bool,
    /// Human message (error or ok).
    pub message: String,
}

/// Overall verification report — ordered stages, fail-closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationReport {
    /// Per-stage results in pipeline order.
    pub stages: Vec<StageResult>,
    /// Overall pass (all stages passed).
    pub passed: bool,
}

impl VerificationReport {
    /// Create a report from stage results (computes overall).
    #[must_use]
    pub fn new(stages: Vec<StageResult>) -> Self {
        let passed = stages.iter().all(|s| s.passed);
        Self { stages, passed }
    }

    /// Whether the report is fully passing.
    #[must_use]
    pub fn is_passed(&self) -> bool {
        self.passed
    }

    /// First failing stage, if any.
    #[must_use]
    pub fn first_failure(&self) -> Option<&StageResult> {
        self.stages.iter().find(|s| !s.passed)
    }
}

// ── 7-stage pure functions ───────────────────────────────────────────────

/// Stage 1: fetch framing budgets.
pub fn check_fetch_framing(
    transfer_bytes: usize,
    elapsed_ms: u64,
    max_bytes: usize,
    max_ms: u64,
) -> Result<(), PackageError> {
    if transfer_bytes > max_bytes {
        return Err(PackageError::Budget {
            message: format!(
                "transfer size {transfer_bytes} exceeds budget {max_bytes} (stage {})",
                VerificationStage::FetchFraming
            ),
        });
    }
    if elapsed_ms > max_ms {
        return Err(PackageError::Budget {
            message: format!(
                "transfer time {elapsed_ms}ms exceeds budget {max_ms}ms (stage {})",
                VerificationStage::FetchFraming
            ),
        });
    }
    Ok(())
}

/// Stage 2: artifact checksum (H-A) — compare fetched bytes digest vs lock record.
pub fn verify_artifact_checksum(
    artifact_bytes: &[u8],
    expected_hex: &str,
) -> Result<(), PackageError> {
    if !is_valid_hex_digest(expected_hex) {
        return Err(PackageError::Integrity {
            stage: VerificationStage::ArtifactChecksum.label().to_string(),
            message: format!("expected digest '{expected_hex}' is not valid 64-hex"),
        });
    }
    let actual = sha256_hex(artifact_bytes);
    if !actual.eq_ignore_ascii_case(expected_hex) {
        return Err(PackageError::DigestMismatch {
            kind: "artifact".to_string(),
            expected: expected_hex.to_ascii_lowercase(),
            actual,
        });
    }
    Ok(())
}

/// Stage 3: manifest validation (delegates to manifest's own validation).
pub fn verify_manifest(manifest: &PackageManifest) -> Result<(), PackageError> {
    manifest.validate().map_err(|e| PackageError::Integrity {
        stage: VerificationStage::ManifestValidation.label().to_string(),
        message: e.to_string(),
    })
}

/// Stage 4: manifest hash binding (H-B) — canonical digest vs lock record.
pub fn verify_manifest_hash_binding(
    manifest: &PackageManifest,
    expected_hex: &str,
) -> Result<(), PackageError> {
    manifest.verify_canonical_digest(expected_hex)
}

/// Stage 5: capability diff — `new_caps` vs `granted_caps` (previously approved set).
///
/// Any addition (`new - granted`) requires explicit approval; narrowing or equal
/// carries forward silently. Returns the added set (empty if no increase).
#[must_use]
pub fn capability_diff<'a>(granted: &[String], new: &'a [String]) -> Vec<&'a String> {
    let granted_set: std::collections::BTreeSet<&str> =
        granted.iter().map(|s| s.as_str()).collect();
    new.iter()
        .filter(|c| !granted_set.contains(c.as_str()))
        .collect()
}

/// Stage 5 check: fail-closed if any added capability without approval.
pub fn check_capability_diff(
    granted: &[String],
    new: &[String],
    approved: bool,
) -> Result<(), PackageError> {
    let added = capability_diff(granted, new);
    if !added.is_empty() && !approved {
        return Err(PackageError::CapabilityIncrease {
            added: added.into_iter().cloned().collect(),
        });
    }
    Ok(())
}

/// Stage 6: compatibility check — host versions must satisfy manifest requirements.
///
/// Minimal check: if manifest declares a requirement, host version must be non-empty
/// and contain a dot; full semver range evaluation is deferred to resolver.
/// For draft, we fail only when host version is absent while manifest requires one,
/// or when version strings are obviously malformed.
pub fn check_compatibility(
    manifest: &PackageManifest,
    host_bitty_version: Option<&str>,
    host_plugin_api_version: Option<&str>,
) -> Result<(), PackageError> {
    if manifest.compat.bitty.is_some() && host_bitty_version.is_none() {
        return Err(PackageError::Incompatible {
            field: "compat.bitty".to_string(),
            message: "host bitty version required but not provided".to_string(),
        });
    }
    if manifest.compat.plugin_api.is_some() && host_plugin_api_version.is_none() {
        return Err(PackageError::Incompatible {
            field: "compat.plugin_api".to_string(),
            message: "host plugin_api version required but not provided".to_string(),
        });
    }
    // If both present, do a lightweight sanity: versions must be semver-like.
    if let Some(v) = host_bitty_version {
        if v.trim().is_empty() || !v.contains('.') {
            return Err(PackageError::Incompatible {
                field: "host.bitty_version".to_string(),
                message: format!("host version '{v}' is malformed"),
            });
        }
    }
    if let Some(v) = host_plugin_api_version {
        if v.trim().is_empty() || !v.contains('.') {
            return Err(PackageError::Incompatible {
                field: "host.plugin_api_version".to_string(),
                message: format!("host version '{v}' is malformed"),
            });
        }
    }
    Ok(())
}

/// Stage 7: store commit — stub headless check that digests are valid hex and
/// the lock resolution would be persisted only after the store commit.
///
/// In production this writes into the content-addressed store under digests;
/// here it validates that digests are well-formed and that content_root, if
/// present, is valid hex.
pub fn verify_store_commit(
    artifact_digest: &str,
    manifest_digest: &str,
    content_root: Option<&str>,
) -> Result<(), PackageError> {
    validate_hex_digest(artifact_digest, "digests.artifact")?;
    validate_hex_digest(manifest_digest, "digests.manifest")?;
    if let Some(r) = content_root {
        validate_hex_digest(r, "digests.content_root")?;
    }
    Ok(())
}

// ── high-level headless pipeline ─────────────────────────────────────────

/// Inputs for the full 7-stage verification pipeline (pure data).
#[derive(Debug, Clone)]
pub struct VerificationInputs<'a> {
    /// Fetched artifact bytes.
    pub artifact_bytes: &'a [u8],
    /// Expected artifact digest (H-A from lock).
    pub expected_artifact_digest: &'a str,
    /// Parsed manifest to validate.
    pub manifest: &'a PackageManifest,
    /// Expected canonical manifest digest (H-B from lock).
    pub expected_manifest_digest: &'a str,
    /// Previously granted capability set (for diff). Empty on first install.
    pub granted_capabilities: &'a [String],
    /// Newly requested capabilities (usually manifest's capability strings).
    pub requested_capabilities: &'a [String],
    /// Whether capability diff approval was given.
    pub capability_approval: bool,
    /// Host bitty version.
    pub host_bitty_version: Option<&'a str>,
    /// Host plugin API version.
    pub host_plugin_api_version: Option<&'a str>,
    /// Expected content root digest (H-C), if the store has landed.
    pub expected_content_root: Option<&'a str>,
    /// Fetch framing budgets.
    pub fetch_bytes: usize,
    /// Fetch elapsed ms.
    pub fetch_elapsed_ms: u64,
    /// Max fetch bytes budget.
    pub max_fetch_bytes: usize,
    /// Max fetch time budget ms.
    pub max_fetch_ms: u64,
}

/// Run all 7 stages in order, collecting per-stage results. Stages after a
/// failure are still checked for reporting, but overall `passed` is false.
#[must_use]
pub fn verify_pipeline(inputs: &VerificationInputs<'_>) -> VerificationReport {
    let mut stages = Vec::with_capacity(7);

    // Stage 1
    let r1 = check_fetch_framing(
        inputs.fetch_bytes,
        inputs.fetch_elapsed_ms,
        inputs.max_fetch_bytes,
        inputs.max_fetch_ms,
    );
    stages.push(StageResult {
        stage: VerificationStage::FetchFraming,
        passed: r1.is_ok(),
        message: r1
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "ok".to_string()),
    });

    // Stage 2
    let r2 = verify_artifact_checksum(inputs.artifact_bytes, inputs.expected_artifact_digest);
    stages.push(StageResult {
        stage: VerificationStage::ArtifactChecksum,
        passed: r2.is_ok(),
        message: r2
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "ok".to_string()),
    });

    // Stage 3
    let r3 = verify_manifest(inputs.manifest);
    stages.push(StageResult {
        stage: VerificationStage::ManifestValidation,
        passed: r3.is_ok(),
        message: r3
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "ok".to_string()),
    });

    // Stage 4
    let r4 = verify_manifest_hash_binding(inputs.manifest, inputs.expected_manifest_digest);
    stages.push(StageResult {
        stage: VerificationStage::ManifestHashBinding,
        passed: r4.is_ok(),
        message: r4
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "ok".to_string()),
    });

    // Stage 5
    let r5 = check_capability_diff(
        inputs.granted_capabilities,
        inputs.requested_capabilities,
        inputs.capability_approval,
    );
    stages.push(StageResult {
        stage: VerificationStage::CapabilityDiff,
        passed: r5.is_ok(),
        message: r5
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "ok".to_string()),
    });

    // Stage 6
    let r6 = check_compatibility(
        inputs.manifest,
        inputs.host_bitty_version,
        inputs.host_plugin_api_version,
    );
    stages.push(StageResult {
        stage: VerificationStage::CompatibilityCheck,
        passed: r6.is_ok(),
        message: r6
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "ok".to_string()),
    });

    // Stage 7
    let r7 = verify_store_commit(
        inputs.expected_artifact_digest,
        inputs.expected_manifest_digest,
        inputs.expected_content_root,
    );
    stages.push(StageResult {
        stage: VerificationStage::StoreCommit,
        passed: r7.is_ok(),
        message: r7
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "ok".to_string()),
    });

    VerificationReport::new(stages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Compat, PackageId, PackageIdentity, PackageManifest};

    fn minimal_manifest() -> PackageManifest {
        PackageManifest {
            identity: PackageIdentity {
                id: PackageId::new("xuepoo.pkg").unwrap(),
                name: "Pkg".to_string(),
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

    #[test]
    fn sha256_known_vectors() {
        // Empty string SHA-256
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        // "abc"
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hex_validation() {
        assert!(is_valid_hex_digest(&"a".repeat(64)));
        assert!(!is_valid_hex_digest("abc"));
        assert!(!is_valid_hex_digest(&"g".repeat(64)));
    }

    #[test]
    fn artifact_checksum_pass_and_fail() {
        let bytes = b"hello artifact";
        let d = sha256_hex(bytes);
        assert!(verify_artifact_checksum(bytes, &d).is_ok());
        assert!(verify_artifact_checksum(bytes, &"a".repeat(64)).is_err());
    }

    #[test]
    fn fetch_framing_enforced() {
        assert!(check_fetch_framing(100, 100, 1000, 1000).is_ok());
        assert!(check_fetch_framing(2000, 100, 1000, 1000).is_err());
        assert!(check_fetch_framing(100, 2000, 1000, 1000).is_err());
    }

    #[test]
    fn capability_diff_detection() {
        let granted = vec!["fs.read".to_string()];
        let new = vec!["fs.read".to_string(), "fs.write".to_string()];
        assert_eq!(capability_diff(&granted, &new).len(), 1);
        assert!(check_capability_diff(&granted, &new, false).is_err());
        assert!(check_capability_diff(&granted, &new, true).is_ok());
        // Narrowing is ok without approval.
        let narrowed = vec!["fs.read".to_string()];
        let broader_granted = vec!["fs.read".to_string(), "fs.write".to_string()];
        assert!(check_capability_diff(&broader_granted, &narrowed, false).is_ok());
    }

    #[test]
    fn pipeline_passes_when_all_ok() {
        let m = minimal_manifest();
        let artifact = b"pkg-bytes";
        let a_digest = sha256_hex(artifact);
        let m_digest = m.canonical_digest();
        let inputs = VerificationInputs {
            artifact_bytes: artifact,
            expected_artifact_digest: &a_digest,
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
        assert!(report.passed, "report failed: {report:?}");
    }

    #[test]
    fn pipeline_fails_on_tampered_artifact() {
        let m = minimal_manifest();
        let artifact = b"pkg-bytes";
        let a_digest = sha256_hex(artifact);
        let m_digest = m.canonical_digest();
        let tampered = b"tampered";
        let inputs = VerificationInputs {
            artifact_bytes: tampered,
            expected_artifact_digest: &a_digest,
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
        assert!(!report.passed);
        assert_eq!(
            report.first_failure().unwrap().stage,
            VerificationStage::ArtifactChecksum
        );
    }
}
