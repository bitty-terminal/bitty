//! Server-generated 128-bit identifiers (contract `D-02`).
//!
//! `daemon_instance_id` and `session_id` are 128-bit values from the operating
//! system CSPRNG, encoded as exactly 32 lowercase hex characters. They are not
//! secrets: the client echoes them for fencing, and a parsed value never
//! carries authority. Only the daemon mints them.
//!
//! Entropy failure is a hard refusal ([`IdError::RandomUnavailable`]); there is
//! no deterministic fallback, no counter-derived id, and no reuse of a value
//! observed on an existing record.

use std::fmt;
use std::str::FromStr;

/// Number of lowercase hex characters in every encoded identifier.
pub const ID_HEX_CHARS: usize = 32;

/// Raw identifier width in bytes.
const ID_BYTES: usize = 16;

/// Bounded retries when a freshly generated value collides with a resolvable
/// record. A 128-bit collision is already negligible; the retry keeps a broken
/// or stubbed entropy source from silently reusing a live id.
pub(crate) const ID_GENERATION_ATTEMPTS: usize = 4;

/// Source of operating-system entropy.
///
/// The registry owns one source for the whole instance so tests can drive the
/// id sequence and prove failure and collision handling deterministically.
pub(crate) trait Entropy {
    /// Fill `destination` completely from the OS CSPRNG, or fail closed.
    fn fill(&mut self, destination: &mut [u8]) -> Result<(), IdError>;
}

/// Production entropy source backed by `getrandom` (already in the workspace
/// lockfile via the existing graph; no second RNG stack).
pub(crate) struct OsEntropy;

impl Entropy for OsEntropy {
    fn fill(&mut self, destination: &mut [u8]) -> Result<(), IdError> {
        getrandom::fill(destination).map_err(|_| IdError::RandomUnavailable)
    }
}

/// An identifier failure. No variant echoes a caller-provided value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdError {
    /// The operating system CSPRNG was unavailable; callers must fail closed.
    RandomUnavailable,
    /// The encoded value was not exactly [`ID_HEX_CHARS`] characters.
    InvalidLength,
    /// The encoded value contained a character outside `[0-9a-f]`, or an
    /// uppercase hex digit, which is not the canonical form.
    InvalidCharacter,
}

impl fmt::Display for IdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RandomUnavailable => formatter.write_str("operating system entropy unavailable"),
            Self::InvalidLength => formatter.write_str("identifier is not 32 hex characters"),
            Self::InvalidCharacter => formatter.write_str("identifier is not lowercase hex"),
        }
    }
}

impl std::error::Error for IdError {}

macro_rules! hex_identifier {
    ($name:ident, $summary:literal) => {
        #[doc = $summary]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; ID_BYTES]);

        impl $name {
            /// The canonical 32-character lowercase hex encoding.
            #[must_use]
            pub fn as_hex(self) -> String {
                encode_hex(&self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in self.0 {
                    write!(formatter, "{byte:02x}")?;
                }
                Ok(())
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                decode_hex(value).map(Self)
            }
        }
    };
}

hex_identifier!(
    DaemonInstanceId,
    "Fresh per-process instance identity (contract `D-02`)."
);
hex_identifier!(
    SessionId,
    "Server-assigned session identity (contract `D-02`)."
);
hex_identifier!(
    AttachId,
    "Server-assigned lease identity (contract `D-02`, `D-09`; \
     `attach_id_bytes` = 32 hex characters)."
);

impl DaemonInstanceId {
    /// Mint a fresh instance id from the OS CSPRNG.
    ///
    /// # Errors
    ///
    /// Returns [`IdError::RandomUnavailable`] when the OS CSPRNG fails. The
    /// daemon must not start with a fallback identity.
    pub fn generate() -> Result<Self, IdError> {
        Self::generate_with(&mut OsEntropy)
    }

    /// Mint a fresh instance id from an injected entropy source.
    pub(crate) fn generate_with(entropy: &mut dyn Entropy) -> Result<Self, IdError> {
        random_bytes(entropy).map(Self)
    }

    /// Parse a canonical instance id.
    ///
    /// # Errors
    ///
    /// Returns [`IdError`] for a non-canonical encoding. A parsed value is
    /// only an echo for comparison; it never authorizes an operation.
    pub fn parse(value: &str) -> Result<Self, IdError> {
        value.parse()
    }
}

impl SessionId {
    /// Mint a fresh session id from an injected entropy source.
    ///
    /// Only the registry calls this; a service layer never accepts a
    /// client-supplied session id as a new identity.
    pub(crate) fn generate_with(entropy: &mut dyn Entropy) -> Result<Self, IdError> {
        random_bytes(entropy).map(Self)
    }

    /// Parse a canonical session id for lookup and fencing.
    ///
    /// # Errors
    ///
    /// Returns [`IdError`] for a non-canonical encoding. Parsing does not
    /// confer authority: the registry still decides whether the record exists
    /// in this instance.
    pub fn parse(value: &str) -> Result<Self, IdError> {
        value.parse()
    }
}

impl AttachId {
    /// Mint a fresh attach id from the OS CSPRNG.
    ///
    /// # Errors
    ///
    /// Returns [`IdError::RandomUnavailable`] when the OS CSPRNG fails; a
    /// lease is never admitted with a fallback identity.
    pub fn generate() -> Result<Self, IdError> {
        Self::generate_with(&mut OsEntropy)
    }

    /// Mint a fresh attach id from an injected entropy source (bitty#2602).
    pub(crate) fn generate_with(entropy: &mut dyn Entropy) -> Result<Self, IdError> {
        random_bytes(entropy).map(Self)
    }

    /// Parse a canonical attach id for lookup and acknowledgement.
    ///
    /// # Errors
    ///
    /// Returns [`IdError`] for a non-canonical encoding. Parsing confers no
    /// authority: the lease layer still decides whether the id names a live
    /// lease.
    pub fn parse(value: &str) -> Result<Self, IdError> {
        value.parse()
    }
}

fn random_bytes(entropy: &mut dyn Entropy) -> Result<[u8; ID_BYTES], IdError> {
    let mut bytes = [0u8; ID_BYTES];
    entropy.fill(&mut bytes)?;
    Ok(bytes)
}

/// Encode `bytes` as lowercase hex.
pub(crate) fn encode_hex(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

/// Mint one random lowercase-hex value of [`ID_HEX_CHARS`] characters.
pub(crate) fn random_hex(entropy: &mut dyn Entropy) -> Result<String, IdError> {
    random_bytes(entropy).map(|bytes| encode_hex(&bytes))
}

fn decode_hex(value: &str) -> Result<[u8; ID_BYTES], IdError> {
    if value.len() != ID_HEX_CHARS {
        return Err(IdError::InvalidLength);
    }
    let mut decoded = [0u8; ID_BYTES];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = decode_nibble(pair[0])?;
        let low = decode_nibble(pair[1])?;
        decoded[index] = (high << 4) | low;
    }
    Ok(decoded)
}

fn decode_nibble(byte: u8) -> Result<u8, IdError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(IdError::InvalidCharacter),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    struct FixedEntropy {
        fill: u8,
        fail: bool,
    }

    impl Entropy for FixedEntropy {
        fn fill(&mut self, destination: &mut [u8]) -> Result<(), IdError> {
            if self.fail {
                return Err(IdError::RandomUnavailable);
            }
            destination.fill(self.fill);
            Ok(())
        }
    }

    #[test]
    fn daemon_instance_ids_are_canonical_and_unique() {
        let mut seen = HashSet::new();
        for _ in 0..4096 {
            let id = DaemonInstanceId::generate().expect("OS entropy is available");
            let text = id.as_hex();
            assert_eq!(text.len(), ID_HEX_CHARS);
            assert_eq!(text, id.to_string());
            assert!(
                text.bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            );
            assert!(
                seen.insert(id),
                "a fresh instance id repeated within one run"
            );
        }
    }

    #[test]
    fn session_ids_are_canonical_and_unique() {
        let mut entropy = OsEntropy;
        let mut seen = HashSet::new();
        for _ in 0..4096 {
            let id = SessionId::generate_with(&mut entropy).expect("OS entropy is available");
            let text = id.as_hex();
            assert_eq!(text.len(), ID_HEX_CHARS);
            assert!(
                text.bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            );
            assert!(
                seen.insert(id),
                "a fresh session id repeated within one run"
            );
        }
    }

    #[test]
    fn attach_ids_are_canonical_and_unique() {
        let mut seen = HashSet::new();
        for _ in 0..4096 {
            let id = AttachId::generate().expect("OS entropy is available");
            let text = id.as_hex();
            assert_eq!(text.len(), ID_HEX_CHARS);
            assert_eq!(text, id.to_string());
            assert!(seen.insert(id), "a fresh attach id repeated within one run");
            assert_eq!(AttachId::parse(&text), Ok(id));
        }
        assert_eq!(AttachId::parse(""), Err(IdError::InvalidLength));
    }

    #[test]
    fn parsing_round_trips_and_rejects_non_canonical_input() {
        let id = DaemonInstanceId::generate().expect("OS entropy is available");
        let text = id.as_hex();
        assert_eq!(DaemonInstanceId::parse(&text), Ok(id));
        assert_eq!(text.parse::<DaemonInstanceId>(), Ok(id));

        assert_eq!(DaemonInstanceId::parse(""), Err(IdError::InvalidLength));
        assert_eq!(
            DaemonInstanceId::parse(&text[..ID_HEX_CHARS - 1]),
            Err(IdError::InvalidLength)
        );
        assert_eq!(
            DaemonInstanceId::parse(&format!("{text}0")),
            Err(IdError::InvalidLength)
        );
        assert_eq!(
            DaemonInstanceId::parse(&text.to_uppercase()),
            Err(IdError::InvalidCharacter)
        );
        let mut non_hex = text.clone();
        non_hex.replace_range(..1, "g");
        assert_eq!(
            DaemonInstanceId::parse(&non_hex),
            Err(IdError::InvalidCharacter)
        );

        let session = SessionId::generate_with(&mut OsEntropy).expect("OS entropy is available");
        assert_eq!(SessionId::parse(&session.as_hex()), Ok(session));
        assert_eq!(
            SessionId::parse(&session.as_hex().to_uppercase()),
            Err(IdError::InvalidCharacter)
        );
    }

    #[test]
    fn generation_fails_closed_when_entropy_is_unavailable() {
        let mut entropy = FixedEntropy {
            fill: 0,
            fail: true,
        };
        assert_eq!(
            DaemonInstanceId::generate_with(&mut entropy),
            Err(IdError::RandomUnavailable)
        );
        assert_eq!(
            SessionId::generate_with(&mut entropy),
            Err(IdError::RandomUnavailable)
        );
        assert_eq!(random_hex(&mut entropy), Err(IdError::RandomUnavailable));
    }

    #[test]
    fn fixed_entropy_produces_the_expected_encoding() {
        let mut entropy = FixedEntropy {
            fill: 0xab,
            fail: false,
        };
        let id = SessionId::generate_with(&mut entropy).expect("fixed entropy succeeds");
        assert_eq!(id.as_hex(), "ab".repeat(ID_BYTES));
    }
}
