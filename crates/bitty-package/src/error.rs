//! Owned error vocabulary for `bitty-package`.

use std::fmt;

/// Stable error class mirroring the RFC and security corpus categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    /// Manifest validation failure.
    Manifest,
    /// Lockfile validation failure.
    Lockfile,
    /// Integrity verification failure (any of the 7 stages).
    Integrity,
    /// Publisher trust failure (V-A / V-B / V-C).
    Trust,
    /// Lifecycle state transition failure.
    Lifecycle,
    /// Source / local-path failure.
    Source,
    /// Activation transaction failure.
    Activation,
    /// Generation / retention / rollback failure.
    Generation,
    /// Budget or bound exceeded (Invariant 7).
    Budget,
}

impl fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Manifest => "manifest",
            Self::Lockfile => "lockfile",
            Self::Integrity => "integrity",
            Self::Trust => "trust",
            Self::Lifecycle => "lifecycle",
            Self::Source => "source",
            Self::Activation => "activation",
            Self::Generation => "generation",
            Self::Budget => "budget",
        };
        f.write_str(label)
    }
}

/// Owned, headless-testable error for every `bitty-package` failure mode.
///
/// All fields are owned `String`/`usize` so errors are cloneable, comparable,
/// and inspectable in tests without I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageError {
    /// A field in the manifest failed validation.
    Manifest {
        /// Dotted field path.
        field: String,
        /// Human-readable reason.
        message: String,
    },
    /// Lockfile validation failure.
    Lockfile {
        /// Reason.
        message: String,
    },
    /// A hard limit was exceeded (Invariant 7).
    LimitExceeded {
        /// Field or resource.
        field: String,
        /// Configured limit.
        limit: usize,
        /// Actual value.
        actual: usize,
    },
    /// Digest mismatch (any of H-A / H-B / H-C).
    DigestMismatch {
        /// Which digest (e.g. `"artifact"`, `"manifest"`, `"content_root"`).
        kind: String,
        /// Expected hex.
        expected: String,
        /// Actual hex.
        actual: String,
    },
    /// Manifest hash binding mismatch.
    ManifestHashMismatch {
        /// Expected hex.
        expected: String,
        /// Actual hex.
        actual: String,
    },
    /// Capability increase blocked pending explicit approval (P0-AC-030).
    CapabilityIncrease {
        /// Added capabilities.
        added: Vec<String>,
    },
    /// Trust pin change requires explicit re-approval (V-B).
    TrustPinChanged {
        /// Package id.
        package: String,
        /// Old identity.
        old: String,
        /// New identity.
        new: String,
    },
    /// Signature verification failed (V-C fail-closed).
    Signature {
        /// Reason.
        message: String,
    },
    /// Incompatible host (Plugin API or Bitty version).
    Incompatible {
        /// Field (e.g. `"compat.bitty"`).
        field: String,
        /// Reason.
        message: String,
    },
    /// Source / local-path violation.
    Source {
        /// Reason.
        message: String,
    },
    /// Local-path drift detected.
    LocalPathDrift {
        /// Package id.
        package: String,
        /// Recorded digest.
        recorded: String,
        /// Current digest.
        current: String,
    },
    /// Lifecycle transition not allowed.
    InvalidState {
        /// Package id or generation id.
        id: String,
        /// Current state label.
        current: String,
        /// Expected state label.
        expected: String,
    },
    /// Registry / duplicate / not-found.
    Duplicate {
        /// Kind (e.g. `"package"`).
        kind: String,
        /// Value.
        value: String,
    },
    /// Referenced package or generation not found.
    NotFound {
        /// Identifier.
        id: String,
    },
    /// Activation transaction failed.
    Activation {
        /// Phase label.
        phase: String,
        /// Reason.
        message: String,
    },
    /// Generation / retention / rollback failure.
    Generation {
        /// Reason.
        message: String,
    },
    /// Fetch framing exceeded budgets.
    Budget {
        /// Reason.
        message: String,
    },
    /// Generic integrity failure.
    Integrity {
        /// Stage label.
        stage: String,
        /// Reason.
        message: String,
    },
}

impl PackageError {
    /// Stable error class for diagnostics grouping.
    #[must_use]
    pub fn error_class(&self) -> ErrorClass {
        match self {
            Self::Manifest { .. } => ErrorClass::Manifest,
            Self::Lockfile { .. } => ErrorClass::Lockfile,
            Self::LimitExceeded { .. } | Self::Budget { .. } => ErrorClass::Budget,
            Self::DigestMismatch { .. }
            | Self::ManifestHashMismatch { .. }
            | Self::CapabilityIncrease { .. }
            | Self::Incompatible { .. }
            | Self::Integrity { .. } => ErrorClass::Integrity,
            Self::TrustPinChanged { .. } | Self::Signature { .. } => ErrorClass::Trust,
            Self::Source { .. } | Self::LocalPathDrift { .. } => ErrorClass::Source,
            Self::InvalidState { .. } | Self::Duplicate { .. } | Self::NotFound { .. } => {
                ErrorClass::Lifecycle
            }
            Self::Activation { .. } => ErrorClass::Activation,
            Self::Generation { .. } => ErrorClass::Generation,
        }
    }

    /// Convenience for manifest field errors.
    #[must_use]
    pub fn manifest(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Manifest {
            field: field.into(),
            message: message.into(),
        }
    }

    /// Convenience for lockfile errors.
    #[must_use]
    pub fn lockfile(message: impl Into<String>) -> Self {
        Self::Lockfile {
            message: message.into(),
        }
    }

    /// Convenience for integrity stage errors.
    #[must_use]
    pub fn integrity(stage: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Integrity {
            stage: stage.into(),
            message: message.into(),
        }
    }

    /// Convenience for trust signature errors.
    #[must_use]
    pub fn signature(message: impl Into<String>) -> Self {
        Self::Signature {
            message: message.into(),
        }
    }

    /// Convenience for source errors.
    #[must_use]
    pub fn source(message: impl Into<String>) -> Self {
        Self::Source {
            message: message.into(),
        }
    }

    /// Convenience for activation errors.
    #[must_use]
    pub fn activation(phase: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Activation {
            phase: phase.into(),
            message: message.into(),
        }
    }

    /// Convenience for generation errors.
    #[must_use]
    pub fn generation(message: impl Into<String>) -> Self {
        Self::Generation {
            message: message.into(),
        }
    }
}

impl fmt::Display for PackageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest { field, message } => write!(f, "manifest {field}: {message}"),
            Self::Lockfile { message } => write!(f, "lockfile: {message}"),
            Self::LimitExceeded {
                field,
                limit,
                actual,
            } => write!(f, "limit exceeded {field}: limit {limit}, actual {actual}"),
            Self::DigestMismatch {
                kind,
                expected,
                actual,
            } => write!(
                f,
                "digest mismatch {kind}: expected {expected}, actual {actual}"
            ),
            Self::ManifestHashMismatch { expected, actual } => write!(
                f,
                "manifest hash mismatch: expected {expected}, actual {actual}"
            ),
            Self::CapabilityIncrease { added } => {
                write!(
                    f,
                    "capability increase requires approval: {}",
                    added.join(", ")
                )
            }
            Self::TrustPinChanged { package, old, new } => write!(
                f,
                "trust pin changed for {package}: {old} -> {new} (re-approval required)"
            ),
            Self::Signature { message } => write!(f, "signature: {message}"),
            Self::Incompatible { field, message } => write!(f, "incompatible {field}: {message}"),
            Self::Source { message } => write!(f, "source: {message}"),
            Self::LocalPathDrift {
                package,
                recorded,
                current,
            } => write!(
                f,
                "local-path drift for {package}: recorded {recorded}, current {current}"
            ),
            Self::InvalidState {
                id,
                current,
                expected,
            } => write!(
                f,
                "invalid state for {id}: current {current}, expected {expected}"
            ),
            Self::Duplicate { kind, value } => write!(f, "duplicate {kind}: {value}"),
            Self::NotFound { id } => write!(f, "not found: {id}"),
            Self::Activation { phase, message } => write!(f, "activation {phase}: {message}"),
            Self::Generation { message } => write!(f, "generation: {message}"),
            Self::Budget { message } => write!(f, "budget: {message}"),
            Self::Integrity { stage, message } => write!(f, "integrity {stage}: {message}"),
        }
    }
}

impl std::error::Error for PackageError {}
