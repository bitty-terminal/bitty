//! The per-session 256-bit control token (contract `D-07`, `D-09`).
//!
//! The token is minted from the operating-system CSPRNG at session creation,
//! returned exactly once in the create response, rotated on every successful
//! claim or transfer, and never stored in the clear. A claim returns the rotated
//! token on the claiming connection because that connection becomes the new
//! controller; a transfer returns it only inside
//! [`TransferDelivery`](super::host::TransferDelivery), addressed to the target
//! lease. The daemon keeps only [`ControlTokenDigest`], a domain-separated
//! SHA-256 digest, and verifies a presented token with a constant-time
//! comparison (`subtle`).
//!
//! `ControlToken` deliberately implements neither `Clone` nor `Display` and its
//! [`std::fmt::Debug`] output is a static redaction. The only way to obtain the
//! secret bytes is [`ControlToken::expose_secret`], which the IPC slice calls
//! once while encoding the create/claim/transfer response. The bytes are
//! zeroized on drop. The digest has no `Debug` implementation at all, so a
//! token-bearing structure cannot acquire one by accident.

use std::fmt;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroize;

use crate::registry::{Entropy, IdError};

/// Domain separator for the control-token digest.
///
/// The token is already 256 bits of OS entropy, so a plain hash would be
/// sufficient; the domain keeps the digest from doubling as a hash of the same
/// bytes in another protocol.
const CONTROL_TOKEN_DIGEST_DOMAIN: &[u8] = b"bittyd/control-token/v1\0";

/// A 256-bit per-session control secret.
///
/// Returned once at create, claim, or transfer. It is bound to one session and
/// confers no authority outside that session; the generation is checked
/// separately on every effectful request.
pub struct ControlToken([u8; Self::BYTES]);

impl ControlToken {
    /// Token width in bytes (256 bits, contract `D-09`).
    pub const BYTES: usize = 32;

    /// Mint a fresh token from an injected entropy source.
    pub(crate) fn generate_with(entropy: &mut dyn Entropy) -> Result<Self, IdError> {
        let mut bytes = [0u8; Self::BYTES];
        entropy.fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    /// Build a token from wire bytes so a presented secret can be verified.
    ///
    /// Construction never grants authority: the digest comparison decides.
    /// Callers must not log, persist, or place the bytes in a child
    /// environment. The receiving `ControlToken` zeroizes its own copy on drop,
    /// but the wire/frame buffer the bytes were decoded from is owned by
    /// bitty#2607 and must be zeroized there as soon as the request that carried
    /// the token is handled; this type cannot reach it.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; Self::BYTES]) -> Self {
        Self(bytes)
    }

    /// Borrow the secret bytes for one response encoding.
    ///
    /// This is the only accessor that reveals the secret. The returned slice is
    /// never logged and never stored outside an owner-private secret store.
    #[must_use]
    pub const fn expose_secret(&self) -> &[u8; Self::BYTES] {
        &self.0
    }
}

impl fmt::Debug for ControlToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ControlToken(<redacted>)")
    }
}

impl Drop for ControlToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// The stored digest of one current control token.
///
/// Only the digest is kept after the create response leaves the process. The
/// type implements no formatting traits so it cannot leak through an accidental
/// `Debug` derive on a surrounding record.
pub(crate) struct ControlTokenDigest([u8; 32]);

impl ControlTokenDigest {
    /// Digest one token for storage.
    pub(crate) fn of(token: &ControlToken) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(CONTROL_TOKEN_DIGEST_DOMAIN);
        hasher.update(token.expose_secret());
        let digest = hasher.finalize();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(digest.as_slice());
        Self(bytes)
    }

    /// Compare a presented token with the stored digest in constant time.
    ///
    /// `subtle::ConstantTimeEq` compares every byte of both 32-byte digests; no
    /// prefix, length, or early-exit branch depends on the secret.
    pub(crate) fn matches(&self, token: &ControlToken) -> bool {
        let candidate = Self::of(token);
        bool::from(self.0.as_slice().ct_eq(candidate.0.as_slice()))
    }

    /// The raw digest bytes, for the digest-only storage test.
    #[cfg(test)]
    pub(crate) const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CounterEntropy {
        next: u8,
    }

    impl Entropy for CounterEntropy {
        fn fill(&mut self, destination: &mut [u8]) -> Result<(), IdError> {
            destination.fill(self.next);
            self.next = self.next.wrapping_add(1);
            Ok(())
        }
    }

    struct FailingEntropy;

    impl Entropy for FailingEntropy {
        fn fill(&mut self, _destination: &mut [u8]) -> Result<(), IdError> {
            Err(IdError::RandomUnavailable)
        }
    }

    fn token(seed: u8) -> ControlToken {
        let mut bytes = [0u8; ControlToken::BYTES];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = seed.wrapping_add(u8::try_from(index).expect("index fits"));
        }
        ControlToken::from_bytes(bytes)
    }

    #[test]
    fn tokens_are_256_bit_and_unique() {
        let mut entropy = CounterEntropy { next: 0 };
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let token = ControlToken::generate_with(&mut entropy).expect("entropy available");
            assert_eq!(token.expose_secret().len(), ControlToken::BYTES);
            assert!(seen.insert(*token.expose_secret()));
        }
    }

    #[test]
    fn entropy_failure_refuses_token_generation() {
        assert_eq!(
            ControlToken::generate_with(&mut FailingEntropy).err(),
            Some(IdError::RandomUnavailable)
        );
    }

    #[test]
    fn debug_never_prints_the_secret() {
        let secret = token(0x5a);
        let rendered = format!("{secret:?}");
        assert_eq!(rendered, "ControlToken(<redacted>)");
        let hex = secret
            .expose_secret()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert!(!rendered.contains(&hex));
    }

    #[test]
    fn the_digest_is_domain_separated_and_never_the_token() {
        let secret = token(7);
        let digest = ControlTokenDigest::of(&secret);
        assert_ne!(digest.as_bytes(), secret.expose_secret());

        let mut plain = Sha256::new();
        plain.update(secret.expose_secret());
        let plain = plain.finalize();
        assert_ne!(digest.as_bytes(), plain.as_slice());

        assert!(digest.matches(&secret));
        assert!(!digest.matches(&token(8)));
    }

    #[test]
    fn verification_rejects_every_single_bit_change() {
        // Exhaustive over the 256 secret bits: a comparison that skipped a byte
        // or short-circuited inside a byte would leave one of these accepted.
        let original = token(0xa5);
        let digest = ControlTokenDigest::of(&original);
        let mut presented = *original.expose_secret();
        for bit in 0..(ControlToken::BYTES * 8) {
            let byte = bit / 8;
            let mask = 1u8 << (bit % 8);
            presented[byte] ^= mask;
            let candidate = ControlToken::from_bytes(presented);
            assert!(!digest.matches(&candidate), "bit {bit} change was accepted");
            presented[byte] ^= mask;
        }
        assert!(digest.matches(&ControlToken::from_bytes(presented)));
    }
}
