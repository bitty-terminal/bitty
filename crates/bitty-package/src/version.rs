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

fn compare_numeric_identifiers(a: &str, b: &str) -> std::cmp::Ordering {
    // Strip leading zeros for canonical numeric comparison (even though validated
    // SemVer numeric identifiers do not have leading zeros except a single '0').
    let a_trimmed = a.trim_start_matches('0');
    let b_trimmed = b.trim_start_matches('0');
    a_trimmed
        .len()
        .cmp(&b_trimmed.len())
        .then_with(|| a_trimmed.cmp(b_trimmed))
}

fn compare_prerelease(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let a_ids: Vec<&str> = a.split('.').collect();
    let b_ids: Vec<&str> = b.split('.').collect();
    for (ai, bi) in a_ids.iter().zip(b_ids.iter()) {
        let a_is_num = ai.bytes().all(|c| c.is_ascii_digit());
        let b_is_num = bi.bytes().all(|c| c.is_ascii_digit());
        let ord = match (a_is_num, b_is_num) {
            // Numeric comparison without lossy integer conversion (PLUG-REG-013)
            (true, true) => compare_numeric_identifiers(ai, bi),
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

    #[test]
    fn compare_numeric_identifiers_edge_cases() {
        use std::cmp::Ordering;
        assert_eq!(compare_numeric_identifiers("0", "0"), Ordering::Equal);
        assert_eq!(compare_numeric_identifiers("0", "1"), Ordering::Less);
        assert_eq!(compare_numeric_identifiers("1", "0"), Ordering::Greater);
        assert_eq!(compare_numeric_identifiers("2", "10"), Ordering::Less);
        assert_eq!(compare_numeric_identifiers("10", "9"), Ordering::Greater);
        assert_eq!(compare_numeric_identifiers("123", "124"), Ordering::Less);
        assert_eq!(compare_numeric_identifiers("124", "123"), Ordering::Greater);
        // Exceeding u64 range
        assert_eq!(
            compare_numeric_identifiers("18446744073709551616", "18446744073709551615"),
            Ordering::Greater
        );
        assert_eq!(
            compare_numeric_identifiers(
                "888888888888888888888888888888",
                "999999999999999999999999999999"
            ),
            Ordering::Less
        );
        assert_eq!(
            compare_numeric_identifiers(
                "1000000000000000000000000000000",
                "999999999999999999999999999999"
            ),
            Ordering::Greater
        );
    }

    #[test]
    fn lossless_numeric_prerelease_ordering_exceeding_u64() {
        // PLUG-REG-013: numeric prerelease identifiers exceeding u64::MAX must not
        // collapse to u64::MAX fallback equality.
        let v_u64_max = Version::parse("1.0.0-18446744073709551615").unwrap();
        let v_u64_plus_1 = Version::parse("1.0.0-18446744073709551616").unwrap();
        assert!(v_u64_max < v_u64_plus_1);
        assert_eq!(
            v_u64_max.cmp_precedence(&v_u64_plus_1),
            std::cmp::Ordering::Less
        );

        let v_large_8 = Version::parse("1.0.0-888888888888888888888888888888").unwrap();
        let v_large_9 = Version::parse("1.0.0-999999999999999999999999999999").unwrap();
        assert!(v_large_8 < v_large_9);
        assert_eq!(
            v_large_8.cmp_precedence(&v_large_9),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            v_large_9.cmp_precedence(&v_large_8),
            std::cmp::Ordering::Greater
        );

        // Different digit counts exceeding u64 range
        let v_len_30 = Version::parse("1.0.0-999999999999999999999999999999").unwrap();
        let v_len_31 = Version::parse("1.0.0-1000000000000000000000000000000").unwrap();
        assert!(v_len_30 < v_len_31);
    }

    #[test]
    fn numeric_prerelease_ordering_properties_symmetry_and_transitivity() {
        use std::cmp::Ordering;

        let raw_versions = [
            "1.0.0-0",
            "1.0.0-1",
            "1.0.0-2",
            "1.0.0-9",
            "1.0.0-10",
            "1.0.0-42",
            "1.0.0-99",
            "1.0.0-100",
            "1.0.0-18446744073709551615", // u64::MAX
            "1.0.0-18446744073709551616", // u64::MAX + 1
            "1.0.0-888888888888888888888888888888",
            "1.0.0-999999999999999999999999999999",
            "1.0.0-1000000000000000000000000000000",
            "1.0.0-9999999999999999999999999999999999999999",
            "1.0.0-9999999999999999999999999999999999999999.1",
            "1.0.0-alpha",
            "1.0.0-alpha.0",
            "1.0.0-alpha.1",
            "1.0.0-alpha.18446744073709551616",
            "1.0.0-alpha.beta",
            "1.0.0",
        ];

        let versions: Vec<Version> = raw_versions
            .iter()
            .map(|raw| Version::parse(raw).unwrap())
            .collect();

        // 1. Reflexivity: v.cmp(v) == Equal
        for (idx, v) in versions.iter().enumerate() {
            assert_eq!(
                v.cmp(v),
                Ordering::Equal,
                "reflexivity failed for version index {idx} ({v})"
            );
        }

        // 2. Strict total ordering & transitivity: for all i < j, v[i] < v[j]
        for i in 0..versions.len() {
            for j in (i + 1)..versions.len() {
                let vi = &versions[i];
                let vj = &versions[j];
                assert_eq!(
                    vi.cmp(vj),
                    Ordering::Less,
                    "expected {vi} < {vj} (index {i} < {j})"
                );
                assert!(vi < vj);

                // 3. Symmetry: v[i].cmp(v[j]) == v[j].cmp(v[i]).reverse()
                assert_eq!(
                    vi.cmp(vj),
                    vj.cmp(vi).reverse(),
                    "symmetry failed between {vi} and {vj}"
                );

                // 4. Transitivity across triplet (i, j, k)
                for vk in versions.iter().skip(j + 1) {
                    assert!(
                        vi < vk,
                        "transitivity failed: {vi} < {vj} and {vj} < {vk} but not {vi} < {vk}"
                    );
                }
            }
        }
    }
}
