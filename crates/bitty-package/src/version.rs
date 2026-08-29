//! Strict SemVer version parsing and precedence for the resolver.
//!
//! Versions are `X.Y.Z` with optional prerelease and build metadata.
//! Build metadata is ignored for precedence. Prerelease precedence follows
//! SemVer 2.0.0 rules. All inputs are bounded (Invariant 7).

#![forbid(unsafe_code)]

use crate::error::PackageError;

/// Maximum version text length (64 bytes) per RFC §Constraint grammar.
pub const MAX_VERSION_LEN: usize = 64;

/// Owned, validated SemVer version.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Version {
    /// Major.
    pub major: u32,
    /// Minor.
    pub minor: u32,
    /// Patch.
    pub patch: u32,
    /// Prerelease string without leading `-`, if present.
    pub prerelease: Option<String>,
    /// Build metadata without leading `+`, if present.
    pub build: Option<String>,
    /// Original validated text.
    pub raw: String,
}

impl Version {
    /// Parse a strict SemVer version string.
    pub fn parse(raw: &str) -> Result<Self, PackageError> {
        if raw.is_empty() {
            return Err(PackageError::manifest(
                "package.version",
                "version must not be empty",
            ));
        }
        if raw.len() > MAX_VERSION_LEN {
            return Err(PackageError::LimitExceeded {
                field: "version".to_string(),
                limit: MAX_VERSION_LEN,
                actual: raw.len(),
            });
        }
        // Allowed characters for version: alphanumeric, ., -, +, _
        for b in raw.bytes() {
            if !(b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+' | b'_')) {
                return Err(PackageError::manifest(
                    "package.version",
                    format!("version '{raw}' contains invalid character"),
                ));
            }
        }
        // Split build metadata first
        let (without_build, build) = match raw.split_once('+') {
            Some((a, b)) => {
                if b.is_empty() {
                    return Err(PackageError::manifest(
                        "package.version",
                        format!("version '{raw}' has empty build metadata"),
                    ));
                }
                // Build must not contain '+'
                if b.contains('+') {
                    return Err(PackageError::manifest(
                        "package.version",
                        format!("version '{raw}' has multiple '+'"),
                    ));
                }
                validate_identifiers(b, "build", false)?;
                (a, Some(b.to_string()))
            }
            None => (raw, None),
        };
        // Split prerelease
        let (core, prerelease) = match without_build.split_once('-') {
            Some((c, pre)) => {
                if pre.is_empty() {
                    return Err(PackageError::manifest(
                        "package.version",
                        format!("version '{raw}' has empty prerelease"),
                    ));
                }
                // prerelease must not contain '+', already split
                validate_identifiers(pre, "prerelease", true)?;
                (c, Some(pre.to_string()))
            }
            None => (without_build, None),
        };
        // Core must be X.Y.Z
        let parts: Vec<&str> = core.split('.').collect();
        if parts.len() != 3 {
            return Err(PackageError::manifest(
                "package.version",
                format!("version '{raw}' must be SemVer X.Y.Z"),
            ));
        }
        let mut nums = [0u32; 3];
        for (idx, part) in parts.iter().enumerate() {
            if part.is_empty() {
                return Err(PackageError::manifest(
                    "package.version",
                    format!("version '{raw}' has empty numeric component"),
                ));
            }
            if !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(PackageError::manifest(
                    "package.version",
                    format!("version '{raw}' numeric components must be digits"),
                ));
            }
            if part.len() > 1 && part.starts_with('0') {
                return Err(PackageError::manifest(
                    "package.version",
                    format!("version '{raw}' must not have leading zeros"),
                ));
            }
            let v: u32 = part.parse().map_err(|_| {
                PackageError::manifest(
                    "package.version",
                    format!("version '{raw}' numeric component out of range"),
                )
            })?;
            nums[idx] = v;
        }
        Ok(Self {
            major: nums[0],
            minor: nums[1],
            patch: nums[2],
            prerelease,
            build,
            raw: raw.to_string(),
        })
    }

    /// True when this version has a prerelease identifier.
    #[must_use]
    pub fn is_prerelease(&self) -> bool {
        self.prerelease.is_some()
    }

    /// Raw string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Core triple equality (X.Y.Z) ignoring prerelease/build.
    #[must_use]
    pub fn core_eq(&self, other: &Self) -> bool {
        self.major == other.major && self.minor == other.minor && self.patch == other.patch
    }

    /// Precedence comparison ignoring build metadata per SemVer.
    /// Returns Ordering for precedence (greater means higher).
    #[must_use]
    pub fn cmp_precedence(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match self.major.cmp(&other.major) {
            Ordering::Equal => {}
            ord => return ord,
        }
        match self.minor.cmp(&other.minor) {
            Ordering::Equal => {}
            ord => return ord,
        }
        match self.patch.cmp(&other.patch) {
            Ordering::Equal => {}
            ord => return ord,
        }
        // Prerelease: absence > presence
        match (&self.prerelease, &other.prerelease) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(a), Some(b)) => compare_prerelease(a, b),
        }
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(Ord::cmp(self, other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.cmp_precedence(other)
    }
}

fn validate_identifiers(
    raw: &str,
    kind: &str,
    check_numeric_leading_zero: bool,
) -> Result<(), PackageError> {
    if raw.is_empty() {
        return Err(PackageError::manifest(
            "package.version",
            format!("{kind} must not be empty"),
        ));
    }
    if raw.len() > 64 {
        return Err(PackageError::LimitExceeded {
            field: format!("version.{kind}"),
            limit: 64,
            actual: raw.len(),
        });
    }
    for id in raw.split('.') {
        if id.is_empty() {
            return Err(PackageError::manifest(
                "package.version",
                format!("{kind} identifier must not be empty in '{raw}'"),
            ));
        }
        for b in id.bytes() {
            if !(b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
                return Err(PackageError::manifest(
                    "package.version",
                    format!("{kind} identifier '{id}' contains invalid character"),
                ));
            }
        }
        if check_numeric_leading_zero
            && id.bytes().all(|b| b.is_ascii_digit())
            && id.len() > 1
            && id.starts_with('0')
        {
            return Err(PackageError::manifest(
                "package.version",
                format!("{kind} numeric identifier '{id}' must not have leading zeros"),
            ));
        }
    }
    Ok(())
}

fn compare_prerelease(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let a_ids: Vec<&str> = a.split('.').collect();
    let b_ids: Vec<&str> = b.split('.').collect();
    for (ai, bi) in a_ids.iter().zip(b_ids.iter()) {
        let a_is_num = ai.bytes().all(|c| c.is_ascii_digit());
        let b_is_num = bi.bytes().all(|c| c.is_ascii_digit());
        let ord = match (a_is_num, b_is_num) {
            (true, true) => {
                // Numeric comparison
                let an: u64 = ai.parse().unwrap_or(u64::MAX);
                let bn: u64 = bi.parse().unwrap_or(u64::MAX);
                an.cmp(&bn)
            }
            (true, false) => Ordering::Less, // numeric has lower precedence
            (false, true) => Ordering::Greater,
            (false, false) => ai.cmp(bi), // lexical ASCII
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    // If all equal so far, longer set has higher precedence
    a_ids.len().cmp(&b_ids.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stable() {
        let v = Version::parse("1.2.3").unwrap();
        assert_eq!(v.major, 1);
        assert_eq!(v.minor, 2);
        assert_eq!(v.patch, 3);
        assert!(!v.is_prerelease());
    }

    #[test]
    fn parse_prerelease_and_build() {
        let v = Version::parse("1.0.0-alpha.1+build.123").unwrap();
        assert!(v.is_prerelease());
        assert_eq!(v.prerelease.as_deref(), Some("alpha.1"));
        assert_eq!(v.build.as_deref(), Some("build.123"));
    }

    #[test]
    fn reject_leading_zero() {
        assert!(Version::parse("01.0.0").is_err());
        assert!(Version::parse("1.01.0").is_err());
        assert!(Version::parse("1.0.0-alpha.01").is_err()); // numeric prerelease leading zero
    }

    #[test]
    fn precedence_stable_gt_prerelease() {
        let stable = Version::parse("1.0.0").unwrap();
        let pre = Version::parse("1.0.0-alpha").unwrap();
        assert!(stable > pre);
    }

    #[test]
    fn prerelease_ordering() {
        let a = Version::parse("1.0.0-alpha.1").unwrap();
        let b = Version::parse("1.0.0-alpha.beta").unwrap();
        assert!(a < b); // numeric < non-numeric
        let c = Version::parse("1.0.0-alpha.1").unwrap();
        let d = Version::parse("1.0.0-alpha.1").unwrap();
        assert_eq!(c.cmp_precedence(&d), std::cmp::Ordering::Equal);
    }

    #[test]
    fn build_ignored() {
        let a = Version::parse("1.0.0+build1").unwrap();
        let b = Version::parse("1.0.0+build2").unwrap();
        assert_eq!(a.cmp_precedence(&b), std::cmp::Ordering::Equal);
    }

    #[test]
    fn max_len_enforced() {
        let long = "1.0.0-".to_string() + &"a".repeat(60);
        assert!(long.len() > 64);
        assert!(Version::parse(&long).is_err());
    }

    #[test]
    fn reject_invalid_char() {
        assert!(Version::parse("1.0.0*").is_err());
    }
}
