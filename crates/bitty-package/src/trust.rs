//! Publisher trust options V-A / V-B / V-C.
//!
//! Checksums prove what was fetched matches what was locked; they do not
//! prove who published it. Three trust models close that gap at increasing
//! infrastructure cost.

use std::collections::BTreeMap;

use crate::error::PackageError;
use crate::integrity::validate_hex_digest;
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
///
/// Reserved wire shape for the future V-C scheme (OQ-029 key-directory
/// design): `signature_hex` holds a 64-byte signature as 128 hex chars
/// (Ed25519-sized). No signature scheme is implemented yet (bitty#743), so
/// [`verify_signature`] rejects every record fail-closed; this type only
/// carries and bounds untrusted input until the scheme lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureRecord {
    /// Key id that supposedly signed this release.
    pub key_id: String,
    /// Signature bytes as 128 hex (reserved 64-byte signature, Ed25519-sized).
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
        // Reserved 64-byte signature shape (128 hex chars). No scheme verifies
        // it yet; the bound only keeps untrusted input well-formed.
        if self.signature_hex.len() != 128
            || !self.signature_hex.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(PackageError::signature(
                "signature must be 128 hex chars (reserved 64-byte signature)",
            ));
        }
        validate_hex_digest(&self.manifest_digest, "signature.manifest_digest")?;
        validate_hex_digest(&self.artifact_digest, "signature.artifact_digest")?;
        Ok(())
    }
}

/// Authenticated key record.
///
/// Reserved wire shape for the future V-C scheme (OQ-029 key-directory
/// design): `public_key_hex` holds a 32-byte public key as 64 hex chars
/// (Ed25519-sized). The store still tracks enrollment and revocation state,
/// but nothing verifies against it until the scheme lands.
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
                "public_key_hex must be 64 hex chars (reserved 32-byte public key)",
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
/// Fail-closed (PL-AC-004, bitty#743 / CTX-0462): V-C signature verification
/// is currently UNAVAILABLE — no signature scheme is implemented, so every
/// record is rejected and `TrustMode::Signed` installs cannot proceed.
///
/// Background: the previous implementation accepted
/// `SHA-256(key_id || manifest_digest || artifact_digest)` as a signature.
/// That value is computable by anyone holding the public `key_id`, so it
/// proved nothing about the publisher and any attacker could forge it. The
/// forgeable check and its mint helper are removed entirely; this function
/// refuses to verify rather than accepting a forgeable record.
///
/// Reserved format for the follow-up real scheme (OQ-029 key-directory
/// design): Ed25519-shaped fields — 32-byte public keys (`public_key_hex`,
/// 64 hex) and 64-byte signatures (`signature_hex`, 128 hex). Malformed
/// input is rejected with a format error; well-formed input is rejected as
/// unverifiable until the scheme lands. Both paths fail closed.
pub fn verify_signature(
    sig: &SignatureRecord,
    _keys: &KeyStore,
    _expected_manifest_digest: &str,
    _expected_artifact_digest: &str,
) -> Result<(), PackageError> {
    // Malformed input stays rejected with a format error (fail-closed).
    sig.validate()?;

    Err(PackageError::signature(
        "V-C signature verification is unavailable: no signature scheme is implemented (bitty#743); refusing to verify rather than accepting a forgeable record",
    ))
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

    // Well-formed record over the expected digests with a trusted key is
    // still rejected: no scheme is implemented, so nothing may verify
    // (bitty#743 / CTX-0462). The error must say verification is unavailable,
    // not that the record was malformed.
    fn well_formed_sig(key_id: &str, m: &str, a: &str) -> SignatureRecord {
        SignatureRecord {
            key_id: key_id.to_string(),
            signature_hex: "d".repeat(128),
            manifest_digest: m.to_string(),
            artifact_digest: a.to_string(),
        }
    }

    #[test]
    fn signature_unavailable_rejects_well_formed_record() {
        let mut keys = KeyStore::new();
        keys.insert(KeyRecord {
            key_id: "k1".to_string(),
            public_key_hex: "a".repeat(64),
            revoked: false,
        })
        .unwrap();
        let m = sha256_hex(b"manifest");
        let a = sha256_hex(b"artifact");
        let sig = well_formed_sig("k1", &m, &a);
        let err = verify_signature(&sig, &keys, &m, &a).unwrap_err();
        assert!(
            err.to_string().contains("unavailable"),
            "expected unavailable fail-closed, got: {err}"
        );
    }

    #[test]
    fn forged_record_with_public_key_id_rejected() {
        // Attacker knows only the public key_id and recomputes the removed
        // stub formula directly — no signing helper exists anymore.
        let mut keys = KeyStore::new();
        keys.insert(KeyRecord {
            key_id: "k1".to_string(),
            public_key_hex: "c".repeat(64),
            revoked: false,
        })
        .unwrap();
        let m = sha256_hex(b"manifest");
        let a = sha256_hex(b"artifact");
        let mut preimage = Vec::new();
        preimage.extend_from_slice(b"k1");
        preimage.extend_from_slice(m.as_bytes());
        preimage.extend_from_slice(a.as_bytes());
        let base = sha256_hex(&preimage);
        let forged = format!("{base}{base}");
        let sig = SignatureRecord {
            key_id: "k1".to_string(),
            signature_hex: forged,
            manifest_digest: m.clone(),
            artifact_digest: a.clone(),
        };
        assert!(verify_signature(&sig, &keys, &m, &a).is_err());
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

        // Unknown key
        let bad_key = well_formed_sig("unknown", &m, &a);
        assert!(verify_signature(&bad_key, &keys, &m, &a).is_err());

        // Record over different bytes than expected
        let m2 = sha256_hex(b"other");
        let sig_over_different = well_formed_sig("k1", &m2, &a);
        assert!(verify_signature(&sig_over_different, &keys, &m, &a).is_err());

        // Revoked key
        keys.revoke("k1").unwrap();
        let sig = well_formed_sig("k1", &m, &a);
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
    fn key_rotation_store_semantics_without_verification() {
        // The key store still tracks enrollment and revocation (needed by the
        // future scheme); verification itself stays unavailable throughout.
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
        assert!(keys.is_trusted("k1"));
        assert!(keys.is_trusted("k2"));
        let m = sha256_hex(b"m");
        let a = sha256_hex(b"a");
        assert!(verify_signature(&well_formed_sig("k2", &m, &a), &keys, &m, &a).is_err());
        keys.revoke("k1").unwrap();
        assert!(!keys.is_trusted("k1"));
        assert!(keys.is_trusted("k2"));
        assert!(verify_signature(&well_formed_sig("k1", &m, &a), &keys, &m, &a).is_err());
        assert!(verify_signature(&well_formed_sig("k2", &m, &a), &keys, &m, &a).is_err());
    }
}
