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

/// Publisher signature over manifest + artifact digests (V-C, OQ-029).
///
/// Wire shape: `signature_hex` holds a 64-byte Ed25519 signature as 128 hex
/// chars, made over [`signing_message`] for the two digests below. The
/// record carries untrusted input — format is bounded here, authenticity is
/// established only by [`verify_signature`] against an enrolled key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureRecord {
    /// Key id that supposedly signed this release.
    pub key_id: String,
    /// Signature bytes as 128 hex (64-byte Ed25519 signature).
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
        // Ed25519 signature shape: exactly 64 bytes as 128 hex chars.
        // Format only — authenticity is established by `verify_signature`.
        if self.signature_hex.len() != 128
            || !self.signature_hex.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(PackageError::signature(
                "signature must be 128 hex chars (64-byte Ed25519 signature)",
            ));
        }
        validate_hex_digest(&self.manifest_digest, "signature.manifest_digest")?;
        validate_hex_digest(&self.artifact_digest, "signature.artifact_digest")?;
        Ok(())
    }
}

/// Authenticated key record: one entry of the V-C key directory (OQ-029).
///
/// `public_key_hex` holds a 32-byte Ed25519 public key as 64 hex chars.
/// The directory tracks enrollment ([`KeyStore::insert`]), revocation
/// ([`KeyStore::revoke`]), and rotation (enroll the successor id, revoke
/// the predecessor); [`verify_signature`] always resolves against the
/// current store state, so signatures from revoked or removed keys fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRecord {
    /// Key id.
    pub key_id: String,
    /// Public key hex (64 hex chars for the 32-byte Ed25519 key).
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
                "public_key_hex must be 64 hex chars (32-byte Ed25519 public key)",
            ));
        }
        Ok(())
    }
}

/// In-memory V-C key directory (OQ-029 enrollment / revocation store).
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

/// Domain separation prefix for the V-C signed message (OQ-029).
///
/// The prefix binds signatures to this package scheme so a signature minted
/// for another protocol cannot verify here, and vice versa.
pub const SIGNING_DOMAIN: &str = "bitty-package-v1";

/// Canonical bytes covered by a V-C signature.
///
/// `bitty-package-v1:<manifest_hex>:<artifact_hex>` — both digests are the
/// 64-hex SHA-256 values from the lock record. Binding both digests ties
/// the signature to one exact manifest and one exact artifact, so a
/// signature cannot be replayed across releases or packages. Publishers
/// sign these bytes with the Ed25519 secret key whose public half is
/// enrolled in the [`KeyStore`] under the record's `key_id`.
#[must_use]
pub fn signing_message(manifest_digest: &str, artifact_digest: &str) -> Vec<u8> {
    let mut msg = Vec::with_capacity(
        SIGNING_DOMAIN.len() + 1 + manifest_digest.len() + 1 + artifact_digest.len(),
    );
    msg.extend_from_slice(SIGNING_DOMAIN.as_bytes());
    msg.push(b':');
    msg.extend_from_slice(manifest_digest.as_bytes());
    msg.push(b':');
    msg.extend_from_slice(artifact_digest.as_bytes());
    msg
}

/// Decode exactly `N` bytes from hex (accepts upper or lower case).
///
/// Length and alphabet are checked here as well as in `validate`, so a
/// caller that skips validation still fails closed.
fn decode_hex<const N: usize>(hex: &str, field: &str) -> Result<[u8; N], PackageError> {
    fn nibble(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    if hex.len() != 2 * N {
        return Err(PackageError::signature(format!(
            "{field} must be {} hex chars",
            2 * N
        )));
    }
    let bytes = hex.as_bytes();
    let mut out = [0u8; N];
    for (i, slot) in out.iter_mut().enumerate() {
        let (Some(hi), Some(lo)) = (nibble(bytes[2 * i]), nibble(bytes[2 * i + 1])) else {
            return Err(PackageError::signature(format!("{field} must be hex")));
        };
        *slot = (hi << 4) | lo;
    }
    Ok(out)
}

/// Verify a signature record against a key store and expected digests.
///
/// Ed25519 (OQ-029, bitty#767) via `ed25519-dalek` (pure Rust, no I/O):
/// the record's `key_id` resolves to an enrolled, non-revoked public key,
/// the record's digests must equal the expected lock digests (no replay
/// across releases), and the signature must verify over
/// [`signing_message`]. Every failure mode is fail-closed (`PL-AC-004`):
/// unknown keys, revoked keys, digest mismatch, malformed keys or
/// signatures, and cryptographic mismatch are all rejected.
///
/// Background: the pre-#743 implementation accepted
/// `SHA-256(key_id || manifest_digest || artifact_digest)` as a signature.
/// That value is computable by anyone holding the public `key_id`, so it
/// proved nothing about the publisher and any attacker could forge it.
/// Forged values of that shape are rejected here because they cannot be
/// valid Ed25519 signatures without the secret key.
pub fn verify_signature(
    sig: &SignatureRecord,
    keys: &KeyStore,
    expected_manifest_digest: &str,
    expected_artifact_digest: &str,
) -> Result<(), PackageError> {
    // Malformed record input stays rejected with a format error.
    sig.validate()?;
    validate_hex_digest(expected_manifest_digest, "expected.manifest_digest")?;
    validate_hex_digest(expected_artifact_digest, "expected.artifact_digest")?;

    // The record must speak for exactly the expected release: otherwise a
    // valid signature could be replayed across digests.
    if sig.manifest_digest != expected_manifest_digest
        || sig.artifact_digest != expected_artifact_digest
    {
        return Err(PackageError::signature(
            "signature digests do not match the expected release digests",
        ));
    }

    // Resolve the key: unknown or revoked keys fail closed.
    let key = keys
        .get(&sig.key_id)
        .ok_or_else(|| PackageError::signature(format!("unknown signing key '{}'", sig.key_id)))?;
    if key.revoked {
        return Err(PackageError::signature(format!(
            "signing key '{}' is revoked",
            sig.key_id
        )));
    }

    let public_bytes: [u8; 32] = decode_hex(&key.public_key_hex, "key.public_key_hex")?;
    let public_key = ed25519_dalek::VerifyingKey::from_bytes(&public_bytes)
        .map_err(|e| PackageError::signature(format!("invalid Ed25519 public key: {e}")))?;
    let signature_bytes: [u8; 64] = decode_hex(&sig.signature_hex, "signature.signature_hex")?;
    let signature = ed25519_dalek::Signature::from_bytes(&signature_bytes);
    let message = signing_message(expected_manifest_digest, expected_artifact_digest);
    use ed25519_dalek::Verifier as _;
    public_key
        .verify(&message, &signature)
        .map_err(|_| PackageError::signature("Ed25519 signature verification failed"))?;
    Ok(())
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

    // Valid Ed25519 round-trip verifies; every forgery or misuse fails
    // closed (bitty#767). Fixed seeds keep the tests deterministic.
    use ed25519_dalek::Signer as _;

    fn test_signing_key(seed_byte: u8) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[seed_byte; 32])
    }

    fn hex_of(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    fn enroll(keys: &mut KeyStore, key_id: &str, signing: &ed25519_dalek::SigningKey) {
        keys.insert(KeyRecord {
            key_id: key_id.to_string(),
            public_key_hex: hex_of(signing.verifying_key().as_bytes()),
            revoked: false,
        })
        .unwrap();
    }

    fn sign_record(
        signing: &ed25519_dalek::SigningKey,
        key_id: &str,
        m: &str,
        a: &str,
    ) -> SignatureRecord {
        let sig = signing.sign(&signing_message(m, a));
        SignatureRecord {
            key_id: key_id.to_string(),
            signature_hex: hex_of(&sig.to_bytes()),
            manifest_digest: m.to_string(),
            artifact_digest: a.to_string(),
        }
    }

    #[test]
    fn signature_valid_round_trip_verifies() {
        let signing = test_signing_key(0x11);
        let mut keys = KeyStore::new();
        enroll(&mut keys, "k1", &signing);
        let m = sha256_hex(b"manifest");
        let a = sha256_hex(b"artifact");
        let sig = sign_record(&signing, "k1", &m, &a);
        assert!(verify_signature(&sig, &keys, &m, &a).is_ok());
    }

    #[test]
    fn forged_record_with_public_key_id_rejected() {
        // Attacker knows only the public key_id and recomputes the removed
        // stub formula directly — without the secret key it cannot be a
        // valid Ed25519 signature.
        let signing = test_signing_key(0x22);
        let mut keys = KeyStore::new();
        enroll(&mut keys, "k1", &signing);
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
    fn signature_wrong_key_id_rejected() {
        // Signature minted by k1 but presented as k2: the bytes do not
        // verify under k2's enrolled public key.
        let k1 = test_signing_key(0x33);
        let k2 = test_signing_key(0x44);
        let mut keys = KeyStore::new();
        enroll(&mut keys, "k1", &k1);
        enroll(&mut keys, "k2", &k2);
        let m = sha256_hex(b"manifest");
        let a = sha256_hex(b"artifact");
        let sig = sign_record(&k1, "k2", &m, &a);
        assert!(verify_signature(&sig, &keys, &m, &a).is_err());
    }

    #[test]
    fn signature_fail_closed() {
        let signing = test_signing_key(0x55);
        let mut keys = KeyStore::new();
        enroll(&mut keys, "k1", &signing);
        let m = sha256_hex(b"manifest");
        let a = sha256_hex(b"artifact");

        // Unknown key
        let bad_key = sign_record(&signing, "unknown", &m, &a);
        assert!(verify_signature(&bad_key, &keys, &m, &a).is_err());

        // Record over different bytes than expected (replay across digests)
        let m2 = sha256_hex(b"other");
        let sig_over_different = sign_record(&signing, "k1", &m2, &a);
        assert!(verify_signature(&sig_over_different, &keys, &m, &a).is_err());

        // Revoked key
        keys.revoke("k1").unwrap();
        let sig = sign_record(&signing, "k1", &m, &a);
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
    fn key_rotation_new_key_valid_old_revoked_stale_fails() {
        // Rotation: enroll successor k2, revoke predecessor k1. Signatures
        // resolve against current directory state, so k2 verifies while
        // k1's signatures stop verifying the moment k1 is revoked.
        let k1 = test_signing_key(0x66);
        let k2 = test_signing_key(0x77);
        let mut keys = KeyStore::new();
        enroll(&mut keys, "k1", &k1);
        enroll(&mut keys, "k2", &k2);
        assert!(keys.is_trusted("k1"));
        assert!(keys.is_trusted("k2"));
        let m = sha256_hex(b"m");
        let a = sha256_hex(b"a");
        let sig_k1 = sign_record(&k1, "k1", &m, &a);
        let sig_k2 = sign_record(&k2, "k2", &m, &a);
        assert!(verify_signature(&sig_k1, &keys, &m, &a).is_ok());
        assert!(verify_signature(&sig_k2, &keys, &m, &a).is_ok());
        keys.revoke("k1").unwrap();
        assert!(!keys.is_trusted("k1"));
        assert!(keys.is_trusted("k2"));
        assert!(verify_signature(&sig_k1, &keys, &m, &a).is_err());
        assert!(verify_signature(&sig_k2, &keys, &m, &a).is_ok());
    }
}
