//! Publisher trust options V-A / V-B / V-C.
//!
//! Checksums prove what was fetched matches what was locked; they do not
//! prove who published it. Three trust models close that gap at increasing
//! infrastructure cost.

use std::collections::BTreeMap;

use crate::error::PackageError;
use crate::integrity::{is_valid_hex_digest, validate_hex_digest};
use crate::manifest::PackageId;

// ── trust mode ───────────────────────────────────────────────────────────

/// Publisher trust option per RFC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TrustMode {
    /// V-A Exact lock pinning plus checksums (floor) — P0, normative.
    PinningOnly,
    /// V-B Trust-on-first-use per publisher identity or source.
    TrustOnFirstUse,
    /// V-C Signed releases verified against an authenticated key record.
    Signed,
}

impl TrustMode {
    /// Human label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::PinningOnly => "V-A",
            Self::TrustOnFirstUse => "V-B",
            Self::Signed => "V-C",
        }
    }
}

impl std::fmt::Display for TrustMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

// ── trust pin (V-B) ──────────────────────────────────────────────────────

/// A stored TOFU pin binding the strongest available identity.
///
/// For registry sources this is a publisher public key id; otherwise the
/// exact source URL plus resolved revision.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TrustPin {
    /// Package id this pin belongs to.
    pub package: PackageId,
    /// Pin identity string (key id or `url@rev` or url alone).
    pub identity: String,
    /// Trust mode that created the pin.
    pub mode: TrustMode,
    /// When first seen (opaque host millis).
    pub first_seen: u64,
}

impl TrustPin {
    /// Validate this pin (bounded, non-empty).
    pub fn validate(&self) -> Result<(), PackageError> {
        if self.identity.trim().is_empty() {
            return Err(PackageError::source("trust pin identity must not be empty"));
        }
        if self.identity.len() > 2048 {
            return Err(PackageError::LimitExceeded {
                field: "trust_pin.identity".to_string(),
                limit: 2048,
                actual: self.identity.len(),
            });
        }
        Ok(())
    }
}

/// In-memory TOFU pin store (stub for persisted file).
#[derive(Debug, Default, Clone)]
pub struct TrustStore {
    pins: BTreeMap<String, TrustPin>,
}

impl TrustStore {
    /// Create empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace a pin (first pin is TOFU anchor).
    pub fn pin(&mut self, pin: TrustPin) -> Result<(), PackageError> {
        pin.validate()?;
        self.pins.insert(pin.package.as_str().to_string(), pin);
        Ok(())
    }

    /// Get pin for package.
    #[must_use]
    pub fn get(&self, id: &PackageId) -> Option<&TrustPin> {
        self.pins.get(id.as_str())
    }

    /// Check whether `candidate_identity` matches the stored pin.
    ///
    /// Returns `Ok(())` when matching or when no pin exists (first install).
    /// Returns `Err(TrustPinChanged)` when the identity changed — caller must
    /// surface a loud security event and require explicit re-approval before
    /// proceeding (PL-AC-003).
    pub fn check(&self, package: &PackageId, candidate_identity: &str) -> Result<(), PackageError> {
        let Some(stored) = self.get(package) else {
            return Ok(());
        };
        if stored.identity == candidate_identity {
            return Ok(());
        }
        Err(PackageError::TrustPinChanged {
            package: package.to_string(),
            old: stored.identity.clone(),
            new: candidate_identity.to_string(),
        })
    }

    /// Explicit re-approval: replace pin after user consented to the change.
    pub fn reapprove(
        &mut self,
        package: PackageId,
        new_identity: String,
        now: u64,
    ) -> Result<(), PackageError> {
        let pin = TrustPin {
            package,
            identity: new_identity,
            mode: TrustMode::TrustOnFirstUse,
            first_seen: now,
        };
        self.pin(pin)
    }

    /// Number of pins.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pins.len()
    }

    /// True when empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
    }
}

// ── signature verification (V-C) ─────────────────────────────────────────

/// Publisher signature over manifest + artifact digests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureRecord {
    /// Key id that supposedly signed this release.
    pub key_id: String,
    /// Signature bytes as 128-hex (64-byte stub signature).
    pub signature_hex: String,
    /// Manifest digest that was signed.
    pub manifest_digest: String,
    /// Artifact digest that was signed.
    pub artifact_digest: String,
}

impl SignatureRecord {
    /// Validate format (hex lengths, digests well-formed).
    pub fn validate(&self) -> Result<(), PackageError> {
        if self.key_id.trim().is_empty() {
            return Err(PackageError::signature(
                "signature key_id must not be empty",
            ));
        }
        if self.key_id.len() > 256 {
            return Err(PackageError::LimitExceeded {
                field: "signature.key_id".to_string(),
                limit: 256,
                actual: self.key_id.len(),
            });
        }
        // Stub signature: 128 hex chars (64 bytes). In real impl this may vary by algorithm.
        if self.signature_hex.len() != 128
            || !self.signature_hex.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(PackageError::signature(
                "signature must be 128 hex chars (stub 64-byte signature)",
            ));
        }
        validate_hex_digest(&self.manifest_digest, "signature.manifest_digest")?;
        validate_hex_digest(&self.artifact_digest, "signature.artifact_digest")?;
        Ok(())
    }
}

/// Authenticated key record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRecord {
    /// Key id.
    pub key_id: String,
    /// Public key hex (64 hex chars stub for 32-byte key).
    pub public_key_hex: String,
    /// Whether the key has been revoked.
    pub revoked: bool,
}

impl KeyRecord {
    /// Validate format.
    pub fn validate(&self) -> Result<(), PackageError> {
        if self.key_id.trim().is_empty() {
            return Err(PackageError::signature(
                "key record key_id must not be empty",
            ));
        }
        if self.public_key_hex.len() != 64
            || !self.public_key_hex.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(PackageError::signature(
                "public_key_hex must be 64 hex chars (stub 32-byte key)",
            ));
        }
        Ok(())
    }
}

/// In-memory key store.
#[derive(Debug, Default, Clone)]
pub struct KeyStore {
    keys: BTreeMap<String, KeyRecord>,
}

impl KeyStore {
    /// Create empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a key record.
    pub fn insert(&mut self, key: KeyRecord) -> Result<(), PackageError> {
        key.validate()?;
        self.keys.insert(key.key_id.clone(), key);
        Ok(())
    }

    /// Get a key record.
    #[must_use]
    pub fn get(&self, key_id: &str) -> Option<&KeyRecord> {
        self.keys.get(key_id)
    }

    /// Mark a key as revoked.
    pub fn revoke(&mut self, key_id: &str) -> Result<(), PackageError> {
        let rec = self
            .keys
            .get_mut(key_id)
            .ok_or_else(|| PackageError::NotFound {
                id: key_id.to_string(),
            })?;
        rec.revoked = true;
        Ok(())
    }

    /// Whether `key_id` is known and not revoked.
    #[must_use]
    pub fn is_trusted(&self, key_id: &str) -> bool {
        self.get(key_id).map(|k| !k.revoked).unwrap_or(false)
    }
}

/// Verify a signature record against a key store and expected digests.
///
/// Fail-closed (PL-AC-004): unsigned artifact, signature over different bytes,
/// unknown key, or revoked key are all rejected before staging and never reach
/// the store. The signature is checked as binding both digests; mismatched
/// digests mean the signature was over different bytes.
///
/// Stub verification: `signature_hex` must equal `sha256_hex(key_id + manifest_digest + artifact_digest)`
/// truncated/extended to 128 hex (first 64 bytes of SHA-256 hex duplicated). This is
/// deterministic and testable without real crypto, but demonstrates the fail-closed property:
/// only a signature produced over exactly those bytes with the correct key verifies.
pub fn verify_signature(
    sig: &SignatureRecord,
    keys: &KeyStore,
    expected_manifest_digest: &str,
    expected_artifact_digest: &str,
) -> Result<(), PackageError> {
    sig.validate()?;

    // Check that the signature was over the expected digests (no different-bytes pass).
    if !sig
        .manifest_digest
        .eq_ignore_ascii_case(expected_manifest_digest)
    {
        return Err(PackageError::signature(format!(
            "signature manifest digest {} does not match expected {}",
            sig.manifest_digest, expected_manifest_digest
        )));
    }
    if !sig
        .artifact_digest
        .eq_ignore_ascii_case(expected_artifact_digest)
    {
        return Err(PackageError::signature(format!(
            "signature artifact digest {} does not match expected {}",
            sig.artifact_digest, expected_artifact_digest
        )));
    }

    // Unknown key.
    let key = keys
        .get(&sig.key_id)
        .ok_or_else(|| PackageError::signature(format!("unknown signing key '{}'", sig.key_id)))?;

    // Revoked key.
    if key.revoked {
        return Err(PackageError::signature(format!(
            "signing key '{}' is revoked",
            sig.key_id
        )));
    }

    // Digest format checks (already validated).
    if !is_valid_hex_digest(expected_manifest_digest)
        || !is_valid_hex_digest(expected_artifact_digest)
    {
        return Err(PackageError::signature(
            "expected digests must be valid 64-hex",
        ));
    }

    // Stub deterministic check: signature must be SHA-256(key_id || manifest || artifact) hex doubled.
    let mut preimage = Vec::new();
    preimage.extend_from_slice(sig.key_id.as_bytes());
    preimage.extend_from_slice(expected_manifest_digest.as_bytes());
    preimage.extend_from_slice(expected_artifact_digest.as_bytes());
    let base = crate::integrity::sha256_hex(&preimage);
    // Extend to 128 hex by repeating base twice and truncating to 128.
    let mut expected_sig = base.clone();
    expected_sig.push_str(&base);
    expected_sig.truncate(128);
    if !sig.signature_hex.eq_ignore_ascii_case(&expected_sig) {
        return Err(PackageError::signature(
            "signature verification failed: signature does not match key and digests",
        ));
    }

    Ok(())
}

/// Produce a stub signature for testing (not a real cryptographic signature).
///
/// Callers that need a valid signature for a trusted key should use this
/// helper; it computes the same deterministic value that `verify_signature`
/// expects, so tests can mint “validly signed” artifacts.
#[must_use]
pub fn stub_sign(key_id: &str, manifest_digest: &str, artifact_digest: &str) -> String {
    let mut preimage = Vec::new();
    preimage.extend_from_slice(key_id.as_bytes());
    preimage.extend_from_slice(manifest_digest.as_bytes());
    preimage.extend_from_slice(artifact_digest.as_bytes());
    let base = crate::integrity::sha256_hex(&preimage);
    let mut sig = base.clone();
    sig.push_str(&base);
    sig.truncate(128);
    sig
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrity::sha256_hex;
    use crate::manifest::PackageId;

    fn pid(s: &str) -> PackageId {
        PackageId::new(s).unwrap()
    }

    #[test]
    fn tofu_happy_path() {
        let mut store = TrustStore::new();
        store
            .pin(TrustPin {
                package: pid("xuepoo.a"),
                identity: "key-abc".to_string(),
                mode: TrustMode::TrustOnFirstUse,
                first_seen: 1,
            })
            .unwrap();
        // Same identity passes.
        store.check(&pid("xuepoo.a"), "key-abc").unwrap();
    }

    #[test]
    fn tofu_pin_change_blocks() {
        let mut store = TrustStore::new();
        store
            .pin(TrustPin {
                package: pid("xuepoo.a"),
                identity: "key-old".to_string(),
                mode: TrustMode::TrustOnFirstUse,
                first_seen: 1,
            })
            .unwrap();
        let err = store.check(&pid("xuepoo.a"), "key-new").unwrap_err();
        assert!(format!("{err}").contains("re-approval required"));
    }

    #[test]
    fn tofu_first_install_no_pin() {
        let store = TrustStore::new();
        // No pin yet — any identity passes (but is then pinned).
        assert!(store.check(&pid("xuepoo.a"), "key-first").is_ok());
    }

    #[test]
    fn tofu_reapprove_updates_pin() {
        let mut store = TrustStore::new();
        store
            .pin(TrustPin {
                package: pid("xuepoo.a"),
                identity: "old".to_string(),
                mode: TrustMode::TrustOnFirstUse,
                first_seen: 1,
            })
            .unwrap();
        store
            .reapprove(pid("xuepoo.a"), "new".to_string(), 2)
            .unwrap();
        assert!(store.check(&pid("xuepoo.a"), "new").is_ok());
    }

    #[test]
    fn signature_valid() {
        let mut keys = KeyStore::new();
        keys.insert(KeyRecord {
            key_id: "k1".to_string(),
            public_key_hex: "a".repeat(64),
            revoked: false,
        })
        .unwrap();
        let m = sha256_hex(b"manifest");
        let a = sha256_hex(b"artifact");
        let sig_hex = stub_sign("k1", &m, &a);
        let sig = SignatureRecord {
            key_id: "k1".to_string(),
            signature_hex: sig_hex,
            manifest_digest: m.clone(),
            artifact_digest: a.clone(),
        };
        verify_signature(&sig, &keys, &m, &a).unwrap();
    }

    #[test]
    fn signature_fail_closed() {
        let mut keys = KeyStore::new();
        keys.insert(KeyRecord {
            key_id: "k1".to_string(),
            public_key_hex: "a".repeat(64),
            revoked: false,
        })
        .unwrap();
        let m = sha256_hex(b"manifest");
        let a = sha256_hex(b"artifact");
        let sig_hex = stub_sign("k1", &m, &a);

        // Unknown key
        let bad_key = SignatureRecord {
            key_id: "unknown".to_string(),
            signature_hex: sig_hex.clone(),
            manifest_digest: m.clone(),
            artifact_digest: a.clone(),
        };
        assert!(verify_signature(&bad_key, &keys, &m, &a).is_err());

        // Signature over different bytes
        let m2 = sha256_hex(b"other");
        let sig_over_different = SignatureRecord {
            key_id: "k1".to_string(),
            signature_hex: sig_hex.clone(),
            manifest_digest: m2.clone(),
            artifact_digest: a.clone(),
        };
        assert!(verify_signature(&sig_over_different, &keys, &m, &a).is_err());

        // Revoked key
        keys.revoke("k1").unwrap();
        let sig = SignatureRecord {
            key_id: "k1".to_string(),
            signature_hex: sig_hex,
            manifest_digest: m.clone(),
            artifact_digest: a.clone(),
        };
        assert!(verify_signature(&sig, &keys, &m, &a).is_err());

        // Unsigned (bad hex length)
        let unsigned = SignatureRecord {
            key_id: "k1".to_string(),
            signature_hex: "bad".to_string(),
            manifest_digest: m.clone(),
            artifact_digest: a.clone(),
        };
        assert!(verify_signature(&unsigned, &keys, &m, &a).is_err());
    }

    #[test]
    fn valid_key_rotated_still_verifies() {
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
        let sig = SignatureRecord {
            key_id: "k2".to_string(),
            signature_hex: stub_sign("k2", &m, &a),
            manifest_digest: m.clone(),
            artifact_digest: a.clone(),
        };
        verify_signature(&sig, &keys, &m, &a).unwrap();
        // k1 still revoked? Not revoked, so also still valid.
        let sig1 = SignatureRecord {
            key_id: "k1".to_string(),
            signature_hex: stub_sign("k1", &m, &a),
            manifest_digest: m.clone(),
            artifact_digest: a.clone(),
        };
        verify_signature(&sig1, &keys, &m, &a).unwrap();
    }
}
