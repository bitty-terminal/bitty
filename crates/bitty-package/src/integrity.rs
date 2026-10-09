//! Integrity verification chain — 7 stages per RFC.
//!
//! Each stage consumes the output of the previous one and none may be
//! skipped for any source type. Bundled and local-path sources use degenerate
//! records rather than exemptions.

use crate::error::PackageError;
use crate::manifest::PackageManifest;
use crate::requirement::VersionReq;
use crate::version::Version;

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
    hex_lower(&sha256(bytes))
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

/// SHA-256 round constants (FIPS 180-4 §4.2.2).
const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256 initial hash values (FIPS 180-4 §5.3.3).
const SHA256_H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// SHA-256 block size in bytes.
const SHA256_BLOCK_BYTES: usize = 64;

/// Offset of the 64-bit message-length field inside the final padded block.
const SHA256_LENGTH_OFFSET: usize = SHA256_BLOCK_BYTES - 8;

/// Incremental SHA-256 (FIPS 180-4; pure Rust, no external crate, no
/// unsafe). Memory is fixed (one 64-byte block plus the state), so a caller
/// can digest an arbitrarily large stream through its own bounded read
/// buffer (DIR-030 D5).
#[derive(Debug, Clone)]
pub struct Sha256Hasher {
    state: [u32; 8],
    block: [u8; SHA256_BLOCK_BYTES],
    block_len: usize,
    total_bytes: u64,
}

impl Default for Sha256Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256Hasher {
    /// Hasher over the empty message.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: SHA256_H0,
            block: [0; SHA256_BLOCK_BYTES],
            block_len: 0,
            total_bytes: 0,
        }
    }

    /// Append `data` to the message.
    pub fn update(&mut self, mut data: &[u8]) {
        self.total_bytes = self.total_bytes.wrapping_add(data.len() as u64);
        if self.block_len > 0 {
            let take = (SHA256_BLOCK_BYTES - self.block_len).min(data.len());
            self.block[self.block_len..self.block_len + take].copy_from_slice(&data[..take]);
            self.block_len += take;
            data = &data[take..];
            if self.block_len < SHA256_BLOCK_BYTES {
                return;
            }
            let block = self.block;
            compress(&mut self.state, &block);
            self.block_len = 0;
        }
        let mut chunks = data.chunks_exact(SHA256_BLOCK_BYTES);
        for chunk in &mut chunks {
            let mut block = [0u8; SHA256_BLOCK_BYTES];
            block.copy_from_slice(chunk);
            compress(&mut self.state, &block);
        }
        let rest = chunks.remainder();
        self.block[..rest.len()].copy_from_slice(rest);
        self.block_len = rest.len();
    }

    /// Bytes appended so far.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.total_bytes
    }

    /// Whether nothing was appended yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total_bytes == 0
    }

    /// Finish the message and return the 32-byte digest.
    #[must_use]
    pub fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.total_bytes.wrapping_mul(8);
        let mut block = [0u8; SHA256_BLOCK_BYTES];
        block[..self.block_len].copy_from_slice(&self.block[..self.block_len]);
        block[self.block_len] = 0x80;
        if self.block_len >= SHA256_LENGTH_OFFSET {
            compress(&mut self.state, &block);
            block = [0u8; SHA256_BLOCK_BYTES];
        }
        block[SHA256_LENGTH_OFFSET..].copy_from_slice(&bit_len.to_be_bytes());
        compress(&mut self.state, &block);
        let mut out = [0u8; 32];
        for (i, val) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&val.to_be_bytes());
        }
        out
    }

    /// Finish the message and return the digest as 64 lowercase hex chars.
    #[must_use]
    pub fn finalize_hex(self) -> String {
        hex_lower(&self.finalize())
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

/// Process one 512-bit block.
fn compress(h: &mut [u32; 8], chunk: &[u8; SHA256_BLOCK_BYTES]) {
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
            .wrapping_add(SHA256_K[i])
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
/// Real semver-range evaluation against the host version using the closed
/// grammar (`VersionReq`, no new parsers or dependencies). Fail-closed: an
/// unparseable requirement or host version is an incompatibility, never a pass.
/// A `None` requirement imposes no constraint; a `Some` requirement with a
/// missing host is an incompatibility.
pub fn check_compatibility(
    manifest: &PackageManifest,
    host_bitty_version: Option<&str>,
    host_plugin_api_version: Option<&str>,
) -> Result<(), PackageError> {
    if let Some(req) = &manifest.compat.bitty {
        let Some(host) = host_bitty_version else {
            return Err(PackageError::Incompatible {
                field: "compat.bitty".to_string(),
                message: "host bitty version required but not provided".to_string(),
            });
        };
        check_single_compat(req, host, "compat.bitty", "host.bitty_version")?;
    }
    if let Some(req) = &manifest.compat.plugin_api {
        let Some(host) = host_plugin_api_version else {
            return Err(PackageError::Incompatible {
                field: "compat.plugin_api".to_string(),
                message: "host plugin_api version required but not provided".to_string(),
            });
        };
        check_single_compat(req, host, "compat.plugin_api", "host.plugin_api_version")?;
    }
    Ok(())
}

/// Evaluate one requirement against one host version, fail-closed.
fn check_single_compat(
    requirement: &str,
    host_version: &str,
    req_field: &str,
    host_field: &str,
) -> Result<(), PackageError> {
    let host = Version::parse(host_version).map_err(|e| PackageError::Incompatible {
        field: host_field.to_string(),
        message: format!("host version '{host_version}' is malformed: {e}"),
    })?;
    let req = VersionReq::parse(requirement).map_err(|e| PackageError::Incompatible {
        field: req_field.to_string(),
        message: format!("requirement '{requirement}' is invalid: {e}"),
    })?;
    if !req.matches(&host) {
        return Err(PackageError::Incompatible {
            field: req_field.to_string(),
            message: format!(
                "host version '{host_version}' does not satisfy requirement '{requirement}'"
            ),
        });
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
                bitty: Some(">=0.5.0,<1.0.0".to_string()),
                plugin_api: Some("^1.0".to_string()),
            },
            dependencies: Vec::new(),
            capabilities: Vec::new(),
            raw_bytes_len: 256,
            undeclared_fields: Vec::new(),
        }
    }

    fn manifest_with_compat(bitty: Option<&str>, plugin_api: Option<&str>) -> PackageManifest {
        let mut m = minimal_manifest();
        m.compat = Compat {
            bitty: bitty.map(|s| s.to_string()),
            plugin_api: plugin_api.map(|s| s.to_string()),
        };
        m
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
    fn streaming_hasher_matches_one_shot_across_splits() {
        // Lengths around the 55/56/64-byte padding edges and multi-block.
        for len in [0usize, 1, 55, 56, 63, 64, 65, 119, 120, 128, 1000] {
            let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
            let expected = sha256_hex(&data);
            for split in [1usize, 3, 7, 64, 65] {
                let mut hasher = Sha256Hasher::new();
                for chunk in data.chunks(split) {
                    hasher.update(chunk);
                }
                assert_eq!(hasher.len(), len as u64);
                assert_eq!(hasher.finalize_hex(), expected, "len {len} split {split}");
            }
        }
        // FIPS 180-4 two-block vector, fed in two uneven parts.
        let mut hasher = Sha256Hasher::new();
        for part in [
            &b"abcdbcdecdefdefgefghfghighijhijkijkljklm"[..],
            b"klmnlmnomnopnopq",
        ] {
            hasher.update(part);
        }
        assert_eq!(
            hasher.finalize_hex(),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert!(Sha256Hasher::default().is_empty());
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

    #[test]
    fn compat_rejects_mismatched_range() {
        // CTX-0466: `>=2.0.0` must not pass on a 0.6.0 host (old
        // emptiness-only check accepted any non-empty dotted string).
        let m = manifest_with_compat(Some(">=2.0.0"), None);
        let err = check_compatibility(&m, Some("0.6.0"), None).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("does not satisfy"), "msg: {msg}");
        assert!(msg.contains(">=2.0.0"), "msg: {msg}");
    }

    #[test]
    fn compat_accepts_matching_ranges_and_legit_installs() {
        let m = manifest_with_compat(Some(">=0.5.0,<1.0.0"), Some("^1.0"));
        assert!(check_compatibility(&m, Some("0.6.0"), Some("1.0.0")).is_ok());
        // Caret upper bound enforced.
        let m2 = manifest_with_compat(Some("^1.0"), None);
        assert!(check_compatibility(&m2, Some("1.5.0"), None).is_ok());
        assert!(check_compatibility(&m2, Some("2.0.0"), None).is_err());
        // Tilde.
        let m3 = manifest_with_compat(Some("~1.2.3"), None);
        assert!(check_compatibility(&m3, Some("1.2.5"), None).is_ok());
        assert!(check_compatibility(&m3, Some("1.3.0"), None).is_err());
        // Bare version means exact.
        let m4 = manifest_with_compat(Some("1.2.3"), None);
        assert!(check_compatibility(&m4, Some("1.2.3"), None).is_ok());
        assert!(check_compatibility(&m4, Some("1.2.4"), None).is_err());
        // No requirements means any host passes.
        let m5 = manifest_with_compat(None, None);
        assert!(check_compatibility(&m5, Some("0.6.0"), Some("1.0.0")).is_ok());
        assert!(check_compatibility(&m5, None, None).is_ok());
    }

    #[test]
    fn compat_fails_closed_on_unparseable() {
        // Unparseable requirements must fail closed, never pass.
        for bad in [
            "",
            "   ",
            "*",
            "||",
            ">=1.0 || <2.0",
            "^1.0, <2.0.0",
            "not-a-version",
            ">=1.0.0; rm -rf",
        ] {
            let m = manifest_with_compat(Some(bad), None);
            assert!(
                check_compatibility(&m, Some("0.6.0"), None).is_err(),
                "requirement '{bad}' must fail closed"
            );
        }
        // Malformed hosts fail closed when a requirement exists.
        let m = manifest_with_compat(Some(">=0.5.0"), None);
        for bad_host in ["", "   ", "notaversion", "01.0.0", "1.0.0*", "1.0"] {
            assert!(
                check_compatibility(&m, Some(bad_host), None).is_err(),
                "host '{bad_host}' must fail closed"
            );
        }
    }

    #[test]
    fn compat_requires_host_when_manifest_requires() {
        let m = manifest_with_compat(Some(">=0.5.0"), None);
        assert!(check_compatibility(&m, None, None).is_err());
        let m2 = manifest_with_compat(None, Some("^1.0"));
        assert!(check_compatibility(&m2, Some("0.6.0"), None).is_err());
        assert!(check_compatibility(&m2, Some("0.6.0"), Some("1.0.0")).is_ok());
    }

    #[test]
    fn compat_pipeline_stage_fails_on_mismatch() {
        // Full pipeline must surface the mismatch at CompatibilityCheck.
        let m = manifest_with_compat(Some(">=2.0.0"), None);
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
            host_plugin_api_version: None,
            expected_content_root: None,
            fetch_bytes: 10,
            fetch_elapsed_ms: 10,
            max_fetch_bytes: 1024,
            max_fetch_ms: 1000,
        };
        let report = verify_pipeline(&inputs);
        assert!(!report.passed);
        let compat = report
            .stages
            .iter()
            .find(|s| s.stage == VerificationStage::CompatibilityCheck)
            .unwrap();
        assert!(!compat.passed, "compat stage must fail: {report:?}");
    }
}
