//! Closed constraint grammar per package-followup RFC §Resolver semantics.
//!
//! Grammar: comma-separated intersection of comparators or caret/tilde expression.
//! No `*`, no `||`. Requirement text at most 128 bytes, version text at most 64.
//! Caret/tilde expand to comparator sets before solving. Whitespace insignificant.

#![forbid(unsafe_code)]

use crate::error::PackageError;
use crate::version::{MAX_VERSION_LEN, Version};

/// Maximum requirement text length per RFC.
pub const MAX_REQUIREMENT_LEN: usize = 128;
/// Maximum comparators per requirement (bounded for solver).
pub const MAX_COMPARATORS: usize = 16;

/// Comparator operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComparatorOp {
    /// `=`
    Equal,
    /// `>`
    Greater,
    /// `>=`
    GreaterEq,
    /// `<`
    Less,
    /// `<=`
    LessEq,
}

impl ComparatorOp {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Equal => "=",
            Self::Greater => ">",
            Self::GreaterEq => ">=",
            Self::Less => "<",
            Self::LessEq => "<=",
        }
    }
}

/// Single comparator `op version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparator {
    /// Operator.
    pub op: ComparatorOp,
    /// Version bound.
    pub version: Version,
}

impl Comparator {
    /// Whether this comparator matches `candidate` (ignores prerelease opt-in;
    /// caller handles that separately).
    #[must_use]
    pub fn matches(&self, candidate: &Version) -> bool {
        let ord = candidate.cmp_precedence(&self.version);
        match self.op {
            ComparatorOp::Equal => ord == std::cmp::Ordering::Equal,
            ComparatorOp::Greater => ord == std::cmp::Ordering::Greater,
            ComparatorOp::GreaterEq => {
                ord == std::cmp::Ordering::Greater || ord == std::cmp::Ordering::Equal
            }
            ComparatorOp::Less => ord == std::cmp::Ordering::Less,
            ComparatorOp::LessEq => {
                ord == std::cmp::Ordering::Less || ord == std::cmp::Ordering::Equal
            }
        }
    }
}

/// Closed version requirement — intersection of comparators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionReq {
    /// Comparators (intersection). Empty means impossible? But we always have at least one.
    pub comparators: Vec<Comparator>,
    /// Original raw text for determinism debugging.
    pub raw: String,
}

impl VersionReq {
    /// Parse a requirement string per closed grammar.
    pub fn parse(raw: &str) -> Result<Self, PackageError> {
        if raw.is_empty() {
            return Err(PackageError::manifest(
                "dependencies.version_req",
                "version requirement must not be empty",
            ));
        }
        if raw.len() > MAX_REQUIREMENT_LEN {
            return Err(PackageError::LimitExceeded {
                field: "dependencies.version_req".to_string(),
                limit: MAX_REQUIREMENT_LEN,
                actual: raw.len(),
            });
        }
        // Closed grammar: deny any character not in allowed set.
        // Allowed: alphanumeric, whitespace, ., -, +, ,, <, >, =, ^, ~
        for b in raw.bytes() {
            if !(b.is_ascii_alphanumeric()
                || b.is_ascii_whitespace()
                || matches!(
                    b,
                    b'.' | b'-' | b'+' | b',' | b'<' | b'>' | b'=' | b'^' | b'~'
                ))
            {
                return Err(PackageError::manifest(
                    "dependencies.version_req",
                    format!("version requirement '{raw}' contains invalid character"),
                ));
            }
        }
        // Also explicitly deny * and | even though above already denies them, for clear message
        if raw.contains('*') {
            return Err(PackageError::manifest(
                "dependencies.version_req",
                "wildcard '*' is not allowed in v1",
            ));
        }
        if raw.contains("||") || raw.contains('|') {
            return Err(PackageError::manifest(
                "dependencies.version_req",
                "disjunction '||' is not allowed in v1",
            ));
        }
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(PackageError::manifest(
                "dependencies.version_req",
                "version requirement must not be empty",
            ));
        }

        // Caret or tilde exclusive form
        if trimmed.starts_with('^') || trimmed.starts_with('~') {
            if trimmed.contains(',') {
                return Err(PackageError::manifest(
                    "dependencies.version_req",
                    "caret/tilde requirement must not contain ','",
                ));
            }
            let is_caret = trimmed.starts_with('^');
            let ver_str = trimmed[1..].trim();
            if ver_str.is_empty() {
                return Err(PackageError::manifest(
                    "dependencies.version_req",
                    "caret/tilde requires a version",
                ));
            }
            if ver_str.len() > MAX_VERSION_LEN {
                return Err(PackageError::LimitExceeded {
                    field: "dependencies.version_req".to_string(),
                    limit: MAX_VERSION_LEN,
                    actual: ver_str.len(),
                });
            }
            // Handle missing patch/minor for ^1.0 style: normalize to X.Y.0
            let normalized = normalize_version_str(ver_str)?;
            let ver = Version::parse(&normalized)?;
            let comparators = if is_caret {
                expand_caret(&ver)?
            } else {
                expand_tilde(&ver)?
            };
            return Ok(Self {
                comparators,
                raw: raw.to_string(),
            });
        }

        // Comparator list: split by ','
        let parts: Vec<&str> = trimmed.split(',').collect();
        if parts.len() > MAX_COMPARATORS {
            return Err(PackageError::LimitExceeded {
                field: "dependencies.version_req.comparators".to_string(),
                limit: MAX_COMPARATORS,
                actual: parts.len(),
            });
        }
        let mut comparators = Vec::with_capacity(parts.len());
        for part in parts {
            let seg = part.trim();
            if seg.is_empty() {
                return Err(PackageError::manifest(
                    "dependencies.version_req",
                    "empty comparator segment",
                ));
            }
            comparators.push(parse_single_comparator(seg)?);
        }
        // Deterministic canonical order: sort comparators by (op label, version precedence)
        // This ensures whitespace/comma ordering does not affect logical equality/determinism.
        comparators.sort_by(|a, b| {
            let ord = a.op.label().cmp(b.op.label());
            if ord != std::cmp::Ordering::Equal {
                return ord;
            }
            a.version.cmp_precedence(&b.version)
        });
        Ok(Self {
            comparators,
            raw: raw.to_string(),
        })
    }

    /// Whether this requirement string text contains a prerelease (has '-')
    #[must_use]
    pub fn has_prerelease(&self) -> bool {
        self.comparators.iter().any(|c| c.version.is_prerelease())
    }

    /// Whether this requirement allows a prerelease candidate with same core X.Y.Z.
    /// Checks if any comparator version has same core and is prerelease.
    #[must_use]
    pub fn allows_prerelease_for(&self, candidate: &Version) -> bool {
        if !candidate.is_prerelease() {
            return true;
        }
        // Need same X.Y.Z core in any comparator's prerelease version
        for comp in &self.comparators {
            if comp.version.is_prerelease() && comp.version.core_eq(candidate) {
                return true;
            }
        }
        false
    }

    /// Whether `candidate` satisfies all comparators.
    #[must_use]
    pub fn matches(&self, candidate: &Version) -> bool {
        for c in &self.comparators {
            if !c.matches(candidate) {
                return false;
            }
        }
        true
    }
}

fn normalize_version_str(raw: &str) -> Result<String, PackageError> {
    // Allow ^1.0 or ~1.2 or ^1 to be normalized to X.Y.Z
    // Count dots in core (before any '-' or '+')
    let core_end = raw.find(['-', '+']).unwrap_or(raw.len());
    let core = &raw[..core_end];
    let suffix = &raw[core_end..];
    let dot_count = core.matches('.').count();
    let normalized_core = match dot_count {
        2 => core.to_string(),
        1 => format!("{core}.0"),
        0 => format!("{core}.0.0"),
        _ => {
            return Err(PackageError::manifest(
                "dependencies.version_req",
                format!("version '{raw}' must be SemVer X.Y.Z (caret/tilde shorthand allowed)"),
            ));
        }
    };
    Ok(format!("{normalized_core}{suffix}"))
}

fn expand_caret(ver: &Version) -> Result<Vec<Comparator>, PackageError> {
    // ^1.2.3 => >=1.2.3 <2.0.0
    // ^0.2.3 => >=0.2.3 <0.3.0
    // ^0.0.3 => =0.0.3
    if ver.major == 0 && ver.minor == 0 {
        // ^0.0.x => exactly that version
        return Ok(vec![Comparator {
            op: ComparatorOp::Equal,
            version: ver.clone(),
        }]);
    }
    let lower = Comparator {
        op: ComparatorOp::GreaterEq,
        version: ver.clone(),
    };
    let upper_ver = if ver.major > 0 {
        // CTX-0493: checked increment; `u32::MAX` has no representable successor.
        let next_major = ver.major.checked_add(1).ok_or_else(|| {
            PackageError::manifest(
                "dependencies.version_req",
                format!(
                    "caret upper bound overflow: major {} has no representable successor",
                    ver.major
                ),
            )
        })?;
        Version::parse(&format!("{next_major}.0.0")).map_err(|e| {
            PackageError::manifest(
                "dependencies.version_req",
                format!("caret upper bound overflow: {e}"),
            )
        })?
    } else {
        // major==0, minor>0
        let next_minor = ver.minor.checked_add(1).ok_or_else(|| {
            PackageError::manifest(
                "dependencies.version_req",
                format!(
                    "caret upper bound overflow: minor {} has no representable successor",
                    ver.minor
                ),
            )
        })?;
        Version::parse(&format!("0.{next_minor}.0")).map_err(|e| {
            PackageError::manifest(
                "dependencies.version_req",
                format!("caret upper bound overflow: {e}"),
            )
        })?
    };
    let upper = Comparator {
        op: ComparatorOp::Less,
        version: upper_ver,
    };
    Ok(vec![lower, upper])
}

fn expand_tilde(ver: &Version) -> Result<Vec<Comparator>, PackageError> {
    // ~1.2.3 => >=1.2.3 <1.3.0
    let lower = Comparator {
        op: ComparatorOp::GreaterEq,
        version: ver.clone(),
    };
    // CTX-0493: checked increment; `u32::MAX` has no representable successor.
    let next_minor = ver.minor.checked_add(1).ok_or_else(|| {
        PackageError::manifest(
            "dependencies.version_req",
            format!(
                "tilde upper bound overflow: minor {} has no representable successor",
                ver.minor
            ),
        )
    })?;
    let upper_ver = Version::parse(&format!("{}.{next_minor}.0", ver.major)).map_err(|e| {
        PackageError::manifest(
            "dependencies.version_req",
            format!("tilde upper bound overflow: {e}"),
        )
    })?;
    let upper = Comparator {
        op: ComparatorOp::Less,
        version: upper_ver,
    };
    Ok(vec![lower, upper])
}

fn parse_single_comparator(seg: &str) -> Result<Comparator, PackageError> {
    let seg = seg.trim();
    // Detect operator prefix: try longest first
    let (op, rest) = if let Some(r) = seg.strip_prefix(">=") {
        (ComparatorOp::GreaterEq, r)
    } else if let Some(r) = seg.strip_prefix("<=") {
        (ComparatorOp::LessEq, r)
    } else if let Some(r) = seg.strip_prefix('>') {
        (ComparatorOp::Greater, r)
    } else if let Some(r) = seg.strip_prefix('<') {
        (ComparatorOp::Less, r)
    } else if let Some(r) = seg.strip_prefix('=') {
        (ComparatorOp::Equal, r)
    } else {
        // Bare version means =
        (ComparatorOp::Equal, seg)
    };
    let ver_str = rest.trim();
    if ver_str.is_empty() {
        return Err(PackageError::manifest(
            "dependencies.version_req",
            format!("comparator '{seg}' missing version"),
        ));
    }
    if ver_str.len() > MAX_VERSION_LEN {
        return Err(PackageError::LimitExceeded {
            field: "dependencies.version_req".to_string(),
            limit: MAX_VERSION_LEN,
            actual: ver_str.len(),
        });
    }
    // Accept caret/tilde-style shorthand (`1.0` => `1.0.0`, `1` => `1.0.0`)
    // for consistency with the caret/tilde path and the manifest doc examples
    // (`>=0.5,<1.0`). Fail-closed on malformed (e.g. four components).
    let normalized = normalize_version_str(ver_str)?;
    let ver = Version::parse(&normalized)?;
    Ok(Comparator { op, version: ver })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::Version;

    #[test]
    fn parse_caret_cases() {
        let r = VersionReq::parse("^1.2.3").unwrap();
        assert_eq!(r.comparators.len(), 2);
        assert_eq!(r.comparators[0].op, ComparatorOp::GreaterEq);
        assert_eq!(r.comparators[0].version.to_string(), "1.2.3");
        assert_eq!(r.comparators[1].op, ComparatorOp::Less);
        assert_eq!(r.comparators[1].version.to_string(), "2.0.0");

        let r2 = VersionReq::parse("^0.2.3").unwrap();
        assert_eq!(r2.comparators[1].version.to_string(), "0.3.0");

        let r3 = VersionReq::parse("^0.0.3").unwrap();
        assert_eq!(r3.comparators.len(), 1);
        assert_eq!(r3.comparators[0].op, ComparatorOp::Equal);
    }

    #[test]
    fn parse_tilde() {
        let r = VersionReq::parse("~1.2.3").unwrap();
        assert_eq!(r.comparators[0].version.to_string(), "1.2.3");
        assert_eq!(r.comparators[1].version.to_string(), "1.3.0");
    }

    #[test]
    fn caret_shorthand() {
        let r = VersionReq::parse("^1.0").unwrap();
        assert_eq!(r.comparators[0].version.to_string(), "1.0.0");
        assert_eq!(r.comparators[1].version.to_string(), "2.0.0");
        let r2 = VersionReq::parse("~1.2").unwrap();
        assert_eq!(r2.comparators[0].version.to_string(), "1.2.0");
        assert_eq!(r2.comparators[1].version.to_string(), "1.3.0");
    }

    #[test]
    fn comparator_list() {
        let r = VersionReq::parse(">=1.4.1, <1.6.0").unwrap();
        assert_eq!(r.comparators.len(), 2);
        let v = Version::parse("1.5.0").unwrap();
        assert!(r.matches(&v));
        let v2 = Version::parse("1.6.0").unwrap();
        assert!(!r.matches(&v2));
    }

    #[test]
    fn bare_version_means_equal() {
        let r = VersionReq::parse("1.2.3").unwrap();
        assert_eq!(r.comparators[0].op, ComparatorOp::Equal);
        let v = Version::parse("1.2.3").unwrap();
        assert!(r.matches(&v));
        let v2 = Version::parse("1.2.4").unwrap();
        assert!(!r.matches(&v2));
    }

    #[test]
    fn reject_wildcard_and_disjunction() {
        assert!(VersionReq::parse("*").is_err());
        assert!(VersionReq::parse("^1.0 || ^2.0").is_err());
        assert!(VersionReq::parse(">=1.0 & <2.0").is_err());
        assert!(VersionReq::parse(">=1.0 || <2.0").is_err());
    }

    #[test]
    fn comma_order_determinism() {
        let r1 = VersionReq::parse(">=1.4.1, <1.6.0").unwrap();
        let r2 = VersionReq::parse("<1.6.0, >=1.4.1").unwrap();
        let v = Version::parse("1.5.0").unwrap();
        assert_eq!(r1.matches(&v), r2.matches(&v));
        // Comparators sorted canonical, so they are equal after sorting
        assert_eq!(r1.comparators, r2.comparators);
    }

    #[test]
    fn oversized_rejected() {
        let long = "a".repeat(129);
        assert!(VersionReq::parse(&long).is_err());
    }

    #[test]
    fn prerelease_opt_in() {
        let req = VersionReq::parse(">=1.0.0-alpha").unwrap();
        let pre = Version::parse("1.0.0-alpha.1").unwrap();
        assert!(req.allows_prerelease_for(&pre));
        // same X.Y.Z required
        let other = Version::parse("1.0.1-alpha.1").unwrap();
        assert!(!req.allows_prerelease_for(&other));

        let req2 = VersionReq::parse("^1.0").unwrap();
        let pre2 = Version::parse("1.1.0-alpha.1").unwrap();
        assert!(!req2.allows_prerelease_for(&pre2));
    }

    #[test]
    fn caret_tilde_with_comma_rejected() {
        assert!(VersionReq::parse("^1.2.3, <2.0.0").is_err());
    }

    #[test]
    fn comparator_shorthand_normalized() {
        // CTX-0466: comparators accept `X.Y`/`X` shorthand like caret/tilde,
        // matching the manifest doc examples (`>=0.5,<1.0`).
        let r = VersionReq::parse(">=0.5,<1.0").unwrap();
        assert_eq!(r.comparators.len(), 2);
        let v = Version::parse("0.6.0").unwrap();
        assert!(r.matches(&v));
        let v2 = Version::parse("1.0.0").unwrap();
        assert!(!r.matches(&v2));
        // Bare shorthand also normalizes.
        let rb = VersionReq::parse("1.0").unwrap();
        assert!(rb.matches(&Version::parse("1.0.0").unwrap()));
        // Four components still rejected fail-closed.
        assert!(VersionReq::parse(">=1.2.3.4").is_err());
    }

    #[test]
    fn comparator_shorthand_zero_pads_reported_ranges() {
        // CTX-0493: the exact in-use plugin spellings resolve as X.Y.0.
        let r = VersionReq::parse(">=2.30").unwrap();
        assert_eq!(r.comparators[0].version.to_string(), "2.30.0");
        assert!(r.matches(&Version::parse("2.30.0").unwrap()));
        assert!(r.matches(&Version::parse("2.31.5").unwrap()));
        assert!(!r.matches(&Version::parse("2.29.9").unwrap()));

        let r2 = VersionReq::parse(">=0.5,<1.0").unwrap();
        assert!(r2.matches(&Version::parse("0.5.0").unwrap()));
        assert!(r2.matches(&Version::parse("0.9.9").unwrap()));
        assert!(!r2.matches(&Version::parse("0.4.9").unwrap()));
        assert!(!r2.matches(&Version::parse("1.0.0").unwrap()));
    }

    #[test]
    fn caret_upper_bound_overflow_is_clean_error() {
        // CTX-0493: `u32::MAX` has no representable successor; never panic.
        for raw in [
            "^4294967295",
            "^4294967295.0.0",
            "^0.4294967295",
            "^0.4294967295.0",
        ] {
            let error = VersionReq::parse(raw)
                .expect_err("unrepresentable caret upper bound must be rejected");
            assert!(error.to_string().contains("overflow"), "{raw}: {error}");
        }
    }

    #[test]
    fn tilde_upper_bound_overflow_is_clean_error() {
        for raw in ["~1.4294967295", "~0.4294967295", "~4294967295.4294967295"] {
            let error = VersionReq::parse(raw)
                .expect_err("unrepresentable tilde upper bound must be rejected");
            assert!(error.to_string().contains("overflow"), "{raw}: {error}");
        }
    }

    #[test]
    fn caret_tilde_boundary_successors_are_accepted() {
        // One below `u32::MAX` still expands to the maximal representable bound.
        let caret = VersionReq::parse("^4294967294").unwrap();
        assert_eq!(caret.comparators[1].version.to_string(), "4294967295.0.0");
        assert!(caret.matches(&Version::parse("4294967294.4294967295.4294967295").unwrap()));
        assert!(!caret.matches(&Version::parse("4294967295.0.0").unwrap()));

        let caret_minor = VersionReq::parse("^0.4294967294").unwrap();
        assert_eq!(
            caret_minor.comparators[1].version.to_string(),
            "0.4294967295.0"
        );
        assert!(caret_minor.matches(&Version::parse("0.4294967294.4294967295").unwrap()));
        assert!(!caret_minor.matches(&Version::parse("0.4294967295.0").unwrap()));

        let tilde = VersionReq::parse("~1.4294967294").unwrap();
        assert_eq!(tilde.comparators[1].version.to_string(), "1.4294967295.0");
        assert!(tilde.matches(&Version::parse("1.4294967294.4294967295").unwrap()));
        assert!(!tilde.matches(&Version::parse("1.4294967295.0").unwrap()));

        // Tilde never increments major, so this stays representable.
        let tilde_major_max = VersionReq::parse("~4294967295.4294967294").unwrap();
        assert_eq!(
            tilde_major_max.comparators[1].version.to_string(),
            "4294967295.4294967295.0"
        );
    }
}
