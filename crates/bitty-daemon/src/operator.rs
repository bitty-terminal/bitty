//! The per-boot operator capability (`D-07`, "Operator capability").
//!
//! The capability is minted at daemon start, written once to an owner-private
//! file in the `0700` runtime directory (file `0600`), and rotated on every
//! start. It gates `daemon.inspect` and `daemon.control`. It is explicitly an
//! **intent gate, not a privilege boundary**: any process running as the same
//! user can read the file, so the daemon never claims it isolates the operator
//! from a same-uid adapter.
//!
//! # Failure shape
//!
//! - Minting happens before the endpoint is bound. If the file cannot be
//!   written, the daemon fails to start; it never runs with a fallback or an
//!   unwritten capability.
//! - Verification compares the presented bearer to the in-memory capability in
//!   constant time. A missing store, a wrong bearer, and a peer-check failure
//!   all collapse to one static `Unauthenticated` refusal at `daemon.negotiate`
//!   and close the connection, so the response never reveals whether the
//!   capability file exists.
//! - The file is never read on the request path. Loading is an operator-tooling
//!   and test entry point; a tampered or missing file at runtime cannot change
//!   the daemon's in-memory verification result, and the daemon does not answer
//!   questions about it.
//!
//! The capability is never placed in an environment, inherited by a worker, put
//! in an audit record, or printed: [`OperatorCapability`] and
//! [`OperatorStore`] have redacted `Debug` output and the bearer is zeroized on
//! drop.

use std::fmt;
use std::path::{Path, PathBuf};

use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::handshake::{SecretBearer, hex};

/// File name of the operator capability inside the runtime directory.
pub const CAPABILITY_FILE_NAME: &str = "bittyd.operator";

/// Bytes in the operator capability (256 bits).
pub const CAPABILITY_BYTES: usize = 32;

/// Largest file the loader will read; one capability plus a newline.
const MAX_CAPABILITY_FILE_BYTES: u64 = 128;

/// A refused operator-capability operation.
///
/// Variants carry a static reason only: no path, no bearer, no file content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorError {
    /// The operating-system CSPRNG failed.
    EntropyUnavailable,
    /// The runtime directory is missing, a symlink, not a directory, owned by
    /// another user, or not `0700`.
    UnsafeDirectory,
    /// The capability file is a symlink, not a regular file, owned by another
    /// user, or not `0600`.
    UnsafeFile,
    /// The capability file does not hold exactly one 64-character lowercase hex
    /// bearer, optionally followed by one newline.
    MalformedFile,
    /// The file operation failed.
    Io,
    /// The cell has no owner-private file mechanism in this slice.
    UnsupportedPlatform,
}

impl fmt::Display for OperatorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EntropyUnavailable => formatter.write_str("operating system entropy unavailable"),
            Self::UnsafeDirectory => formatter
                .write_str("operator runtime directory is not a private owned 0700 directory"),
            Self::UnsafeFile => {
                formatter.write_str("operator capability file is not a private owned 0600 file")
            }
            Self::MalformedFile => formatter.write_str("operator capability file is malformed"),
            Self::Io => formatter.write_str("operator capability file operation failed"),
            Self::UnsupportedPlatform => {
                formatter.write_str("this platform cell has no operator capability file")
            }
        }
    }
}

impl std::error::Error for OperatorError {}

/// A minted 256-bit operator capability.
///
/// The value is not [`Copy`], is zeroized on drop, and has a redacted
/// [`fmt::Debug`]. There is no `Display`.
pub struct OperatorCapability {
    bytes: Zeroizing<[u8; CAPABILITY_BYTES]>,
}

impl OperatorCapability {
    /// Mint a fresh capability from the operating-system CSPRNG.
    ///
    /// # Errors
    ///
    /// Returns [`OperatorError::EntropyUnavailable`] when the CSPRNG fails;
    /// there is no fallback capability and no predictable value.
    pub fn generate() -> Result<Self, OperatorError> {
        let mut bytes = Zeroizing::new([0u8; CAPABILITY_BYTES]);
        getrandom::fill(bytes.as_mut()).map_err(|_| OperatorError::EntropyUnavailable)?;
        Ok(Self { bytes })
    }

    /// Parse the canonical 64-character lowercase hex form.
    ///
    /// # Errors
    ///
    /// Returns [`OperatorError::MalformedFile`] for any other form.
    pub fn parse(value: &str) -> Result<Self, OperatorError> {
        let bearer = SecretBearer::parse(value).map_err(|_| OperatorError::MalformedFile)?;
        Ok(Self::from_bearer(&bearer))
    }

    /// Build from a validated bearer.
    #[must_use]
    pub fn from_bearer(bearer: &SecretBearer) -> Self {
        Self {
            bytes: Zeroizing::new(*bearer.expose()),
        }
    }

    /// The capability as a wire bearer (operator tooling and tests only).
    #[must_use]
    pub fn to_bearer(&self) -> SecretBearer {
        SecretBearer::from_bytes(*self.bytes)
    }

    /// The canonical 64-character lowercase hex form, for the owner-private
    /// file. The caller must not log the result.
    #[must_use]
    pub fn as_hex(&self) -> String {
        hex(&*self.bytes)
    }

    /// Verify a presented bearer in constant time.
    #[must_use]
    pub fn verify(&self, presented: &SecretBearer) -> bool {
        self.bytes
            .as_slice()
            .ct_eq(presented.expose().as_slice())
            .into()
    }
}

impl fmt::Debug for OperatorCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OperatorCapability(<redacted>)")
    }
}

/// The daemon's in-memory operator capability.
///
/// One store belongs to one daemon lifetime. Verification never touches the
/// filesystem, so no request path can observe whether the file exists.
pub struct OperatorStore {
    capability: OperatorCapability,
}

impl OperatorStore {
    /// Mint, persist, and hold this daemon run's capability.
    ///
    /// The runtime directory must already exist as this user's `0700` directory
    /// (the endpoint module creates it). The file is rewritten with `0600` and
    /// `O_NOFOLLOW`, so a previous capability is rotated out on every start.
    ///
    /// # Errors
    ///
    /// Returns [`OperatorError::UnsafeDirectory`], [`OperatorError::UnsafeFile`],
    /// [`OperatorError::Io`], or [`OperatorError::EntropyUnavailable`]. The
    /// daemon must not start on any of them.
    #[cfg(unix)]
    pub fn create_in(runtime_dir: &Path) -> Result<Self, OperatorError> {
        use std::fs::{OpenOptions, Permissions};
        use std::io::Write as _;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

        let directory = std::fs::symlink_metadata(runtime_dir).map_err(|_| OperatorError::Io)?;
        let uid = nix::unistd::geteuid().as_raw();
        if !directory.is_dir()
            || directory.file_type().is_symlink()
            || directory.uid() != uid
            || directory.mode() & 0o777 != 0o700
        {
            return Err(OperatorError::UnsafeDirectory);
        }
        let path = capability_path(runtime_dir);
        if let Ok(existing) = std::fs::symlink_metadata(&path) {
            if existing.file_type().is_symlink() || !existing.is_file() || existing.uid() != uid {
                return Err(OperatorError::UnsafeFile);
            }
        }
        let capability = OperatorCapability::generate()?;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|_| OperatorError::Io)?;
        file.set_permissions(Permissions::from_mode(0o600))
            .map_err(|_| OperatorError::Io)?;
        file.write_all(capability.as_hex().as_bytes())
            .map_err(|_| OperatorError::Io)?;
        file.write_all(b"\n").map_err(|_| OperatorError::Io)?;
        file.sync_all().map_err(|_| OperatorError::Io)?;
        Ok(Self { capability })
    }

    /// Mint, persist, and hold this daemon run's capability.
    ///
    /// # Errors
    ///
    /// Always [`OperatorError::UnsupportedPlatform`]: this cell has no
    /// owner-private file mechanism in the slice.
    #[cfg(not(unix))]
    pub fn create_in(_runtime_dir: &Path) -> Result<Self, OperatorError> {
        Err(OperatorError::UnsupportedPlatform)
    }

    /// Load an existing capability from an owner-private file.
    ///
    /// This is the operator-tooling and test path. The daemon never calls it on
    /// a request; see the module docs.
    ///
    /// # Errors
    ///
    /// Returns [`OperatorError::UnsafeDirectory`] / `UnsafeFile` for an owner
    /// or mode mismatch, [`OperatorError::MalformedFile`] for bad content, and
    /// [`OperatorError::Io`] for a filesystem failure.
    #[cfg(unix)]
    pub fn load_from(runtime_dir: &Path) -> Result<Self, OperatorError> {
        use std::io::Read as _;
        use std::os::unix::fs::MetadataExt;

        let directory = std::fs::symlink_metadata(runtime_dir).map_err(|_| OperatorError::Io)?;
        let uid = nix::unistd::geteuid().as_raw();
        if !directory.is_dir()
            || directory.file_type().is_symlink()
            || directory.uid() != uid
            || directory.mode() & 0o777 != 0o700
        {
            return Err(OperatorError::UnsafeDirectory);
        }
        let path = capability_path(runtime_dir);
        let metadata = std::fs::symlink_metadata(&path).map_err(|_| OperatorError::Io)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.uid() != uid
            || metadata.mode() & 0o777 != 0o600
        {
            return Err(OperatorError::UnsafeFile);
        }
        let file = std::fs::File::open(&path).map_err(|_| OperatorError::Io)?;
        let mut bytes = Vec::new();
        file.take(MAX_CAPABILITY_FILE_BYTES)
            .read_to_end(&mut bytes)
            .map_err(|_| OperatorError::Io)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| OperatorError::MalformedFile)?;
        let text = text.strip_suffix('\n').unwrap_or(text);
        let capability = OperatorCapability::parse(text)?;
        Ok(Self { capability })
    }

    /// Load an existing capability from an owner-private file.
    ///
    /// # Errors
    ///
    /// Always [`OperatorError::UnsupportedPlatform`].
    #[cfg(not(unix))]
    pub fn load_from(_runtime_dir: &Path) -> Result<Self, OperatorError> {
        Err(OperatorError::UnsupportedPlatform)
    }

    /// Verify a presented operator capability.
    ///
    /// A missing or malformed request value never reaches this method; the
    /// handshake maps that case to the same refusal as `false`.
    #[must_use]
    pub fn verify(&self, presented: &SecretBearer) -> bool {
        self.capability.verify(presented)
    }

    /// The held capability (operator tooling and tests only).
    #[must_use]
    pub fn capability(&self) -> &OperatorCapability {
        &self.capability
    }
}

impl fmt::Debug for OperatorStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OperatorStore(<redacted>)")
    }
}

/// The capability file path inside a runtime directory.
#[must_use]
pub fn capability_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(CAPABILITY_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    #[cfg(unix)]
    fn private_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("chmod 0700");
        dir
    }

    #[cfg(unix)]
    #[test]
    fn mint_writes_one_owner_private_capability_and_rotates_it() {
        let dir = private_dir();
        let first = OperatorStore::create_in(dir.path()).expect("mint");
        let path = capability_path(dir.path());
        let metadata = std::fs::symlink_metadata(&path).expect("capability file");
        assert!(metadata.is_file());
        assert_eq!(metadata.mode() & 0o777, 0o600);

        let loaded = OperatorStore::load_from(dir.path()).expect("load");
        assert!(loaded.verify(&first.capability().to_bearer()));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            format!("{}\n", first.capability().as_hex())
        );

        let second = OperatorStore::create_in(dir.path()).expect("rotate");
        assert!(!second.capability().verify(&first.capability().to_bearer()));
        assert!(second.capability().verify(&second.capability().to_bearer()));
    }

    #[cfg(unix)]
    #[test]
    fn a_wrong_bearer_is_the_only_observable_result() {
        let dir = private_dir();
        let store = OperatorStore::create_in(dir.path()).expect("mint");
        assert!(store.verify(&store.capability().to_bearer()));
        assert!(!store.verify(&SecretBearer::from_bytes([0u8; 32])));
        assert!(!store.verify(&SecretBearer::from_bytes([0xffu8; 32])));
        let debug = format!("{store:?}");
        assert_eq!(debug, "OperatorStore(<redacted>)");
        assert!(!debug.contains(&store.capability().as_hex()));
        assert_eq!(
            format!("{:?}", store.capability()),
            "OperatorCapability(<redacted>)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_file_is_reported_but_never_reaches_a_request() {
        let dir = private_dir();
        assert_eq!(
            OperatorStore::load_from(dir.path()).expect_err("missing"),
            OperatorError::Io
        );
        // The daemon's own mint always writes the file; the in-memory store
        // therefore never consults the filesystem while serving.
        let store = OperatorStore::create_in(dir.path()).expect("mint");
        let capability = store.capability().to_bearer();
        std::fs::remove_file(capability_path(dir.path())).expect("remove");
        assert!(store.verify(&capability), "verification is in memory");
    }

    #[cfg(unix)]
    #[test]
    fn mode_and_owner_drift_are_refused() {
        let dir = private_dir();
        OperatorStore::create_in(dir.path()).expect("mint");
        let path = capability_path(dir.path());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("chmod 0644");
        assert_eq!(
            OperatorStore::load_from(dir.path()).expect_err("mode drift"),
            OperatorError::UnsafeFile
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 0600");
        assert!(OperatorStore::load_from(dir.path()).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_shared_runtime_directory_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod 0755");
        assert_eq!(
            OperatorStore::create_in(dir.path()).expect_err("shared directory"),
            OperatorError::UnsafeDirectory
        );
        assert_eq!(
            OperatorStore::load_from(dir.path()).expect_err("shared directory"),
            OperatorError::UnsafeDirectory
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_capability_file_is_refused() {
        let dir = private_dir();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, format!("{}\n", "0".repeat(64))).expect("target");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        symlink(&target, capability_path(dir.path())).expect("symlink");
        assert_eq!(
            OperatorStore::load_from(dir.path()).expect_err("symlink"),
            OperatorError::UnsafeFile
        );
        assert_eq!(
            OperatorStore::create_in(dir.path()).expect_err("symlink"),
            OperatorError::UnsafeFile
        );
    }

    #[cfg(unix)]
    #[test]
    fn malformed_content_is_refused_without_echoing_it() {
        let dir = private_dir();
        let path = capability_path(dir.path());
        for content in [
            "short\n",
            &"A".repeat(64),
            &format!("{}g\n", "0".repeat(63)),
        ] {
            std::fs::write(&path, content).expect("write");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
            let error = OperatorStore::load_from(dir.path()).expect_err("malformed");
            assert_eq!(error, OperatorError::MalformedFile);
            assert!(!error.to_string().contains('A'));
        }
    }

    #[test]
    fn capability_parsing_is_canonical() {
        let hex = "0123456789abcdef".repeat(4);
        let capability = OperatorCapability::parse(&hex).expect("valid");
        assert_eq!(capability.as_hex(), hex);
        assert!(OperatorCapability::parse(&hex.to_uppercase()).is_err());
        assert!(OperatorCapability::parse(&hex[..62]).is_err());
    }
}
