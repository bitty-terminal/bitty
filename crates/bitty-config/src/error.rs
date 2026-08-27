//! Owned error vocabulary for `bitty-config`.
//!
//! All failures are plain data — no I/O, no panics, no `unsafe`. Every
//! variant is constructed for headless unit testing and for deterministic
//! diagnostics mirroring the proposed diagnostics contract of the Lua Runtime
//! RFC (severity + stable error class + source location).

use std::fmt;

/// Stable error class for diagnostics (mirrors the Lua Runtime RFC's
/// `syntax` / `resolution` / `validation` / `runtime` / `budget` set, but
/// scoped to the configuration pipeline).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    /// Schema or field validation failure.
    Validation,
    /// Layer merge conflict or precedence violation.
    Merge,
    /// Policy non-overridable violation.
    Policy,
    /// Profile resolution / cycle.
    Resolution,
    /// Schema version or migration failure.
    Migration,
    /// Project trust failure.
    Trust,
    /// Reload classification rejected the plan.
    Reload,
}

impl fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Validation => "validation",
            Self::Merge => "merge",
            Self::Policy => "policy",
            Self::Resolution => "resolution",
            Self::Migration => "migration",
            Self::Trust => "trust",
            Self::Reload => "reload",
        };
        f.write_str(s)
    }
}

/// Owned, headless-testable error for every `bitty-config` failure mode.
///
/// All fields are owned `String` so the error can be cloned, compared, and
/// inspected in tests without allocating outside the crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A typed field failed validation.
    Validation {
        /// Dotted field path (e.g. `"font.size"`).
        field: String,
        /// Human-readable reason, developer-facing English, bounded.
        message: String,
    },
    /// An undeclared / unknown field was present.
    UndeclaredField {
        /// Dotted field path.
        field: String,
        /// Source that introduced the field, if known.
        source: Option<String>,
    },
    /// Two or more layers supplied conflicting values for the same field.
    MergeConflict {
        /// Field that conflicted.
        field: String,
        /// Human-readable source descriptions for each layer.
        sources: Vec<String>,
    },
    /// A system-policy field marked non-overridable was overridden.
    NonOverridable {
        /// Field that was overridden.
        field: String,
        /// Policy source description.
        policy_source: String,
        /// Attempting source description.
        attempted_source: String,
    },
    /// A profile `extends` chain contained a cycle.
    CycleDetected {
        /// Chain that cycled, in visitation order.
        chain: Vec<String>,
    },
    /// A requested `extends` profile was not found.
    ProfileNotFound {
        /// Requested profile name.
        name: String,
    },
    /// Schema version is newer than this crate understands.
    SchemaVersionUnsupported {
        /// Found version.
        found: u32,
        /// Maximum supported version.
        supported: u32,
    },
    /// Migration from one schema version to another failed.
    MigrationFailed {
        /// Source version.
        from: u32,
        /// Target version.
        to: u32,
        /// Reason.
        message: String,
    },
    /// Project trust check failed.
    TrustViolation {
        /// Reason.
        message: String,
    },
    /// Reload rejected the plan.
    ReloadRejected {
        /// Reason.
        message: String,
    },
    /// Generic invalid input (e.g. content hash mismatch).
    InvalidInput {
        /// Reason.
        message: String,
    },
}

impl ConfigError {
    /// Stable error class for diagnostics grouping.
    #[must_use]
    pub fn error_class(&self) -> ErrorClass {
        match self {
            Self::Validation { .. } | Self::UndeclaredField { .. } => ErrorClass::Validation,
            Self::MergeConflict { .. } => ErrorClass::Merge,
            Self::NonOverridable { .. } => ErrorClass::Policy,
            Self::CycleDetected { .. } | Self::ProfileNotFound { .. } => ErrorClass::Resolution,
            Self::SchemaVersionUnsupported { .. } | Self::MigrationFailed { .. } => {
                ErrorClass::Migration
            }
            Self::TrustViolation { .. } | Self::InvalidInput { .. } => ErrorClass::Trust,
            Self::ReloadRejected { .. } => ErrorClass::Reload,
        }
    }

    /// Dotted field name if the error is field-scoped.
    #[must_use]
    pub fn field(&self) -> Option<&str> {
        match self {
            Self::Validation { field, .. }
            | Self::UndeclaredField { field, .. }
            | Self::MergeConflict { field, .. }
            | Self::NonOverridable { field, .. } => Some(field),
            _ => None,
        }
    }

    /// Convenience constructor for validation failures.
    pub fn validation(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Validation {
            field: field.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation { field, message } => {
                write!(f, "validation error at '{field}': {message}")
            }
            Self::UndeclaredField { field, source } => {
                if let Some(s) = source {
                    write!(f, "undeclared field '{field}' from {s}")
                } else {
                    write!(f, "undeclared field '{field}'")
                }
            }
            Self::MergeConflict { field, sources } => {
                write!(
                    f,
                    "merge conflict at '{field}' between sources: {}",
                    sources.join(", ")
                )
            }
            Self::NonOverridable {
                field,
                policy_source,
                attempted_source,
            } => write!(
                f,
                "non-overridable policy field '{field}' from {policy_source} cannot be overridden by {attempted_source}"
            ),
            Self::CycleDetected { chain } => {
                write!(f, "profile extends cycle detected: {}", chain.join(" -> "))
            }
            Self::ProfileNotFound { name } => write!(f, "profile not found: '{name}'"),
            Self::SchemaVersionUnsupported { found, supported } => write!(
                f,
                "schema version {found} is not supported (max {supported})"
            ),
            Self::MigrationFailed { from, to, message } => {
                write!(f, "migration {from} -> {to} failed: {message}")
            }
            Self::TrustViolation { message } => write!(f, "trust violation: {message}"),
            Self::ReloadRejected { message } => write!(f, "reload rejected: {message}"),
            Self::InvalidInput { message } => write!(f, "invalid input: {message}"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_validation() {
        let e = ConfigError::validation("font.size", "must be finite");
        assert_eq!(e.error_class(), ErrorClass::Validation);
        assert_eq!(e.field(), Some("font.size"));
        assert!(e.to_string().contains("font.size"));
    }

    #[test]
    fn display_cycle() {
        let e = ConfigError::CycleDetected {
            chain: vec!["a".into(), "b".into(), "a".into()],
        };
        assert!(e.to_string().contains("cycle"));
        assert_eq!(e.error_class(), ErrorClass::Resolution);
    }

    #[test]
    fn non_overridable_class() {
        let e = ConfigError::NonOverridable {
            field: "window.opacity".into(),
            policy_source: "system/policy.lua".into(),
            attempted_source: "user/init.lua".into(),
        };
        assert_eq!(e.error_class(), ErrorClass::Policy);
        assert!(e.to_string().contains("non-overridable"));
    }
}
