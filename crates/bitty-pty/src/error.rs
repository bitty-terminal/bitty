//! Owned error type for [`crate`].
//!
//! Errors reported by the wrapped upstream PTY layer are flattened into owned
//! data before they cross back into this crate's API; no upstream error type
//! ever escapes the crate (ADR-0004). Genuine operating-system failures that
//! originate inside this crate surface as [`PtyError::Io`].

use std::fmt;

/// Everything that can go wrong across the PTY lifecycle owned by this crate.
#[derive(Debug)]
pub enum PtyError {
    /// The requested operation has no implemented backend on this platform
    /// (for example ConPTY on Windows before the Tier-1 Windows slice).
    Unsupported(&'static str),
    /// The configured program path/name was empty.
    EmptyProgram,
    /// An environment variable entry failed validation.
    InvalidEnvVar {
        /// Offending variable name.
        key: String,
        /// Why the entry was rejected.
        reason: &'static str,
    },
    /// Requested terminal size was outside the supported >= 1x1 grid.
    InvalidSize {
        /// Rejected column count.
        cols: u16,
        /// Rejected row count.
        rows: u16,
    },
    /// Working directory was rejected before spawn.
    InvalidCwd(String),
    /// Underlying operating-system error raised inside this crate.
    Io(std::io::Error),
    /// Opaque failure reported by the wrapped upstream PTY layer. The message
    /// carries the upstream error's primary display text; the upstream type
    /// itself never crosses this crate's API boundary.
    Upstream(String),
    /// The child has already been reaped; no further status is available.
    ChildAlreadyReaped,
    /// A single-use resource (reader or writer half) was already taken.
    HalfAlreadyTaken(&'static str),
}

impl PtyError {
    /// Flattens an upstream failure into owned data. The upstream error type
    /// is intentionally unnamed; callers pass whatever the wrapped layer
    /// returned and only its `Display` output survives.
    // This helper is fully exercised on Unix via `platform::unix`; on
    // Windows the ConPTY seam returns `PtyError::Unsupported` directly until
    // the Tier-1 Windows follow-up slice (ADR-0002) implements the backend,
    // so the helper is intentionally unused on `cfg(windows)`.
    #[cfg_attr(windows, allow(dead_code))]
    pub(crate) fn flatten_upstream<E: fmt::Display>(err: E) -> Self {
        PtyError::Upstream(err.to_string())
    }
}

impl fmt::Display for PtyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PtyError::Unsupported(what) => write!(f, "unsupported on this platform: {what}"),
            PtyError::EmptyProgram => write!(f, "program path must not be empty"),
            PtyError::InvalidEnvVar { key, reason } => {
                write!(f, "invalid environment variable {key:?}: {reason}")
            }
            PtyError::InvalidSize { cols, rows } => {
                write!(f, "invalid terminal size {cols}x{rows}: both must be >= 1")
            }
            PtyError::InvalidCwd(path) => write!(f, "invalid working directory: {path}"),
            PtyError::Io(err) => write!(f, "pty i/o error: {err}"),
            PtyError::Upstream(msg) => write!(f, "pty backend error: {msg}"),
            PtyError::ChildAlreadyReaped => write!(f, "child process status already reaped"),
            PtyError::HalfAlreadyTaken(what) => write!(f, "{what} half already taken"),
        }
    }
}

impl std::error::Error for PtyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PtyError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for PtyError {
    fn from(err: std::io::Error) -> Self {
        PtyError::Io(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn display_covers_variants_without_panicking() {
        let cases = [
            PtyError::Unsupported("ConPTY not implemented yet"),
            PtyError::EmptyProgram,
            PtyError::InvalidEnvVar {
                key: "A=B".to_owned(),
                reason: "must not contain '='",
            },
            PtyError::InvalidSize { cols: 0, rows: 24 },
            PtyError::InvalidCwd("/gone".to_owned()),
            PtyError::Io(std::io::Error::from(std::io::ErrorKind::NotFound)),
            PtyError::Upstream("opaque".to_owned()),
            PtyError::ChildAlreadyReaped,
            PtyError::HalfAlreadyTaken("reader"),
        ];
        for err in cases {
            assert!(!err.to_string().is_empty());
        }
    }

    #[test]
    fn io_errors_keep_source_chain() {
        let inner = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let err = PtyError::from(inner);
        assert!(err.source().is_some());
        assert!(matches!(err, PtyError::Io(_)));
    }

    #[test]
    fn upstream_errors_are_flattened_to_owned_text() {
        let err = PtyError::flatten_upstream("opaque upstream failure");
        match err {
            PtyError::Upstream(msg) => assert_eq!(msg, "opaque upstream failure"),
            other => panic!("expected Upstream, got {other:?}"),
        }
    }
}
