//! The `bittyd` local endpoint: discovery, single-instance lock, tamper
//! checks, and clean shutdown (contract `D-15`, "Endpoint discovery", and
//! "Stale handles, peer re-check, and refusals").
//!
//! # Endpoint rule
//!
//! On Unix the endpoint is `$XDG_RUNTIME_DIR/bitty/bittyd.sock`: a `0600`
//! socket owned by the user inside a `0700` directory owned by the user.
//! `BITTYD_ENDPOINT` overrides the socket path for tests, alternate runtime
//! roots, and nested sessions. It is a path, not a credential: the daemon and
//! a client resolve it the same way, and the owner/mode checks still run when
//! the override is set. An unset or unusable `XDG_RUNTIME_DIR` is
//! [`EndpointError::NoEndpoint`]; there is no `/tmp` fallback and no abstract
//! namespace socket.
//!
//! # Single instance
//!
//! The daemon opens [`LOCK_FILE_NAME`] in the same runtime directory and takes
//! a non-blocking exclusive `flock` for its whole process lifetime. A second
//! start fails with [`EndpointError::AlreadyRunning`] and touches nothing.
//! The kernel releases the lock when the owning process dies, so a crash can
//! leave only a stale socket file; the next start reclaims it after it owns
//! the lock. The lock file is never unlinked: unlinking a lock file lets a new
//! instance create a fresh inode while an older instance still holds the old
//! one, which would defeat single-instance.
//!
//! # Stale sockets
//!
//! An existing endpoint path is first attested (no symlink, a socket, owned by
//! this uid, mode `0600`). A non-blocking connect then classifies it: a
//! successful connect, or `EAGAIN` from a full accept queue, means the
//! endpoint is live and is **never** unlinked; `ECONNREFUSED` (or a path that
//! vanished) is stale and is reclaimed by renaming it to a private quarantine
//! name, re-checking the quarantined object's identity, and only then removing
//! it. Any other probe error is ambiguous and refuses takeover. This module
//! never removes a path it did not prove stale and owned.
//!
//! # Tamper checks
//!
//! Startup attests the runtime directory and socket; [`BoundEndpoint::reattest`]
//! repeats the check for the bounded tamper interval the service loop owns. A
//! mismatch is [`EndpointError::Tampered`], the contract's exit-code `6` path
//! (`D-05`): it is not retried through a weaker transport.
//!
//! # Windows
//!
//! Windows has a designed named-pipe transport, not an implementation in this
//! slice: `\\.\pipe\bittyd-<SID>` with a current-user-only DACL, remote clients
//! rejected at the pipe level, `GetNamedPipeClientProcessId` plus token SID
//! comparison, and fail-closed SID/AppContainer/integrity-level mismatches.
//! [`open_endpoint`] returns [`EndpointError::UnsupportedPlatform`] on Windows
//! until that transport and its native-runner conformance exist.
//!
//! This module binds a listener but never accepts on it: the `bitty-ipc` v1
//! service loop (bitty#2607) owns `accept`, framing, and dispatch.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

#[cfg(unix)]
use std::path::Path;

#[cfg(unix)]
use std::fs::{File, Metadata, OpenOptions, Permissions};
#[cfg(unix)]
use std::os::fd::{AsRawFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::UnixListener;

#[cfg(unix)]
use nix::errno::Errno;
#[cfg(unix)]
use nix::fcntl::{FcntlArg, FdFlag, Flock, FlockArg, OFlag, fcntl};
#[cfg(unix)]
use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, connect, socket};
#[cfg(unix)]
#[cfg(unix)]
use nix::unistd::getuid;

use crate::platform::{EndpointTransport, PlatformCell};

/// Environment variable that overrides the derived endpoint socket path.
pub const ENDPOINT_ENV: &str = "BITTYD_ENDPOINT";

/// Environment variable that anchors the derived endpoint path.
pub const XDG_RUNTIME_DIR_ENV: &str = "XDG_RUNTIME_DIR";

/// Owner-private directory created under the runtime root.
pub const RUNTIME_SUBDIR: &str = "bitty";

/// Socket file name inside [`RUNTIME_SUBDIR`].
pub const SOCKET_FILE_NAME: &str = "bittyd.sock";

/// Single-instance lock file name inside [`RUNTIME_SUBDIR`].
pub const LOCK_FILE_NAME: &str = "bittyd.lock";

/// Runtime-directory mode (`0700`, owner only).
pub const RUNTIME_DIR_MODE: u32 = 0o700;

/// Endpoint socket mode (`0600`, owner read/write only).
pub const SOCKET_MODE: u32 = 0o600;

/// Lock file mode (`0600`, owner read/write only).
pub const LOCK_FILE_MODE: u32 = 0o600;

/// Maximum socket path length in bytes, excluding the terminating NUL.
///
/// Linux `sun_path` is 108 bytes, macOS and the BSDs use 104. The value is
/// checked before `bind`; an over-long endpoint is refused, never truncated.
#[cfg(target_os = "linux")]
pub const MAX_SOCKET_PATH_BYTES: usize = 107;

/// Maximum socket path length in bytes, excluding the terminating NUL.
#[cfg(not(target_os = "linux"))]
pub const MAX_SOCKET_PATH_BYTES: usize = 103;

/// Endpoint resolution inputs, passed explicitly instead of read from the
/// process environment so tests and clients can resolve the same way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EndpointEnv {
    xdg_runtime_dir: Option<OsString>,
    endpoint_override: Option<OsString>,
}

impl EndpointEnv {
    /// Construct resolution inputs. Empty values are treated as unset.
    #[must_use]
    pub fn new(xdg_runtime_dir: Option<OsString>, endpoint_override: Option<OsString>) -> Self {
        Self {
            xdg_runtime_dir,
            endpoint_override,
        }
    }

    /// Read `XDG_RUNTIME_DIR` and `BITTYD_ENDPOINT` from the current process.
    #[must_use]
    pub fn from_process() -> Self {
        Self::new(
            std::env::var_os(XDG_RUNTIME_DIR_ENV),
            std::env::var_os(ENDPOINT_ENV),
        )
    }

    /// The configured runtime root, when set.
    #[must_use]
    pub fn xdg_runtime_dir(&self) -> Option<&std::ffi::OsStr> {
        self.xdg_runtime_dir
            .as_deref()
            .filter(|value| !value.is_empty())
    }

    /// The configured endpoint override, when set.
    #[must_use]
    pub fn endpoint_override(&self) -> Option<&std::ffi::OsStr> {
        self.endpoint_override
            .as_deref()
            .filter(|value| !value.is_empty())
    }
}

/// A typed endpoint failure.
///
/// `UnsupportedPlatform` maps to `Denied/UnsupportedPlatform`, tamper and peer
/// failures map to `Unauthenticated`, and a tamper is the contract's exit-code
/// `6` path; the service loop (bitty#2607) owns the process-exit mapping.
#[derive(Debug)]
pub enum EndpointError {
    /// The compiled cell has no accepted endpoint transport (`D-15`).
    UnsupportedPlatform {
        /// The refused cell.
        cell: PlatformCell,
    },
    /// No endpoint can be derived, with no fallback transport.
    NoEndpoint {
        /// Why the endpoint is unavailable.
        reason: String,
    },
    /// A configured path is not usable as an endpoint path.
    InvalidPath {
        /// The refused path.
        path: PathBuf,
        /// Why it is refused.
        reason: String,
    },
    /// The socket path exceeds the platform's `sun_path` bound.
    PathTooLong {
        /// The refused path.
        path: PathBuf,
        /// Path length in bytes.
        bytes: usize,
        /// Platform maximum in bytes, excluding the NUL.
        max: usize,
    },
    /// Another instance already holds the single-instance lock.
    AlreadyRunning {
        /// The endpoint socket of the running instance.
        socket: PathBuf,
        /// The held lock file.
        lock: PathBuf,
    },
    /// The endpoint or runtime directory failed an ownership, mode, or type
    /// check.
    Tampered {
        /// The offending path.
        path: PathBuf,
        /// What failed.
        kind: TamperKind,
    },
    /// An existing endpoint could not be proven stale, so takeover is refused.
    StaleUnverifiable {
        /// The endpoint path.
        path: PathBuf,
        /// Why the classification failed.
        reason: String,
    },
    /// A filesystem operation failed.
    Io {
        /// The attempted operation.
        action: &'static str,
        /// The affected path.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
}

impl EndpointError {
    /// The static contract result code for this failure, when the contract
    /// assigns one. Startup and filesystem failures (`NoEndpoint`,
    /// `AlreadyRunning`, `Io`, ...) are not service results: a client never
    /// sees them as a wire code.
    #[must_use]
    pub const fn result_code(&self) -> Option<&'static str> {
        match self {
            Self::UnsupportedPlatform { .. } => Some("UnsupportedPlatform"),
            Self::Tampered { .. } => Some("Unauthenticated"),
            Self::NoEndpoint { .. }
            | Self::InvalidPath { .. }
            | Self::PathTooLong { .. }
            | Self::AlreadyRunning { .. }
            | Self::StaleUnverifiable { .. }
            | Self::Io { .. } => None,
        }
    }

    /// Whether this failure is an endpoint tamper (contract exit code `6`).
    #[must_use]
    pub const fn is_tamper(&self) -> bool {
        matches!(self, Self::Tampered { .. })
    }
}

impl fmt::Display for EndpointError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform { cell } => write!(
                formatter,
                "no accepted bittyd endpoint transport on {cell}; \
                 there is no fallback socket, anonymous transport, or TCP listener"
            ),
            Self::NoEndpoint { reason } => write!(formatter, "no bittyd endpoint: {reason}"),
            Self::InvalidPath { path, reason } => {
                write!(
                    formatter,
                    "invalid bittyd endpoint path {}: {reason}",
                    path.display()
                )
            }
            Self::PathTooLong { path, bytes, max } => write!(
                formatter,
                "bittyd endpoint path {} is {bytes} bytes; the platform limit is {max} bytes, \
                 so the path is refused instead of truncated",
                path.display()
            ),
            Self::AlreadyRunning { socket, lock } => write!(
                formatter,
                "bittyd is already running for this user: {} is served and {} is locked",
                socket.display(),
                lock.display()
            ),
            Self::Tampered { path, kind } => {
                write!(formatter, "endpoint tamper at {}: {kind}", path.display())
            }
            Self::StaleUnverifiable { path, reason } => write!(
                formatter,
                "existing endpoint {} cannot be proven stale: {reason}",
                path.display()
            ),
            Self::Io {
                action,
                path,
                source,
            } => write!(formatter, "{action} at {}: {source}", path.display()),
        }
    }
}

impl std::error::Error for EndpointError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// What failed an endpoint attestation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TamperKind {
    /// The path is a symbolic link.
    Symlink,
    /// The path is not a Unix-domain socket.
    NotASocket,
    /// The path is not a directory.
    NotADirectory,
    /// The path is not a regular file.
    NotARegularFile,
    /// The path is owned by another uid.
    ForeignOwner {
        /// The observed owner.
        owner: u32,
        /// The expected owner (the daemon's uid).
        expected: u32,
    },
    /// The path has a mode other than the contract mode.
    WrongMode {
        /// The observed permission bits.
        mode: u32,
        /// The expected permission bits.
        expected: u32,
    },
    /// The bound endpoint was replaced by another object.
    Replaced,
    /// An existing endpoint that could not be classified safely.
    ProbeAmbiguous,
}

impl fmt::Display for TamperKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Symlink => formatter.write_str("path is a symbolic link"),
            Self::NotASocket => formatter.write_str("path is not a socket"),
            Self::NotADirectory => formatter.write_str("path is not a directory"),
            Self::NotARegularFile => formatter.write_str("path is not a regular file"),
            Self::ForeignOwner { owner, expected } => {
                write!(formatter, "owner {owner} is not the daemon uid {expected}")
            }
            Self::WrongMode { mode, expected } => {
                write!(formatter, "mode {mode:o} is not the required {expected:o}")
            }
            Self::Replaced => formatter.write_str("endpoint was replaced by another object"),
            Self::ProbeAmbiguous => {
                formatter.write_str("liveness probe was ambiguous; refusing takeover")
            }
        }
    }
}

/// Resolved endpoint paths, derived by the daemon and reusable by clients.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointPaths {
    runtime_dir: PathBuf,
    socket: PathBuf,
    lock: PathBuf,
}

#[cfg(unix)]
impl EndpointPaths {
    /// Resolve the endpoint paths from explicit inputs.
    ///
    /// An override names the socket path directly; the derived form is
    /// `$XDG_RUNTIME_DIR/bitty/bittyd.sock`. The length bound is checked here,
    /// before any directory or socket is created.
    ///
    /// # Errors
    ///
    /// - [`EndpointError::NoEndpoint`] when no runtime root is configured or
    ///   the root is not absolute. [`open_endpoint`] additionally reports a
    ///   missing or unusable runtime root as `NoEndpoint`.
    /// - [`EndpointError::InvalidPath`] when the override is not an absolute
    ///   path with a file name.
    /// - [`EndpointError::PathTooLong`] when the socket path exceeds
    ///   [`MAX_SOCKET_PATH_BYTES`].
    pub fn resolve(env: &EndpointEnv) -> Result<Self, EndpointError> {
        let (runtime_dir, socket) = match env.endpoint_override() {
            Some(override_path) => {
                let path = PathBuf::from(override_path);
                if !path.is_absolute() {
                    return Err(EndpointError::InvalidPath {
                        path,
                        reason: format!("{ENDPOINT_ENV} must be an absolute path"),
                    });
                }
                if path.file_name().is_none() {
                    return Err(EndpointError::InvalidPath {
                        path,
                        reason: format!("{ENDPOINT_ENV} must name a socket file"),
                    });
                }
                let parent = path.parent().ok_or_else(|| EndpointError::InvalidPath {
                    path: path.clone(),
                    reason: "the endpoint override has no parent directory".to_string(),
                })?;
                (parent.to_path_buf(), path)
            }
            None => {
                let Some(root) = env.xdg_runtime_dir() else {
                    return Err(EndpointError::NoEndpoint {
                        reason: format!(
                            "{XDG_RUNTIME_DIR_ENV} is unset; there is no /tmp or \
                             abstract-namespace fallback"
                        ),
                    });
                };
                let root = PathBuf::from(root);
                if !root.is_absolute() {
                    return Err(EndpointError::NoEndpoint {
                        reason: format!("{XDG_RUNTIME_DIR_ENV} is not an absolute path"),
                    });
                }
                let runtime_dir = root.join(RUNTIME_SUBDIR);
                let socket = runtime_dir.join(SOCKET_FILE_NAME);
                (runtime_dir, socket)
            }
        };

        let bytes = socket.as_os_str().as_encoded_bytes().len();
        if bytes > MAX_SOCKET_PATH_BYTES {
            return Err(EndpointError::PathTooLong {
                path: socket,
                bytes,
                max: MAX_SOCKET_PATH_BYTES,
            });
        }

        let lock = runtime_dir.join(LOCK_FILE_NAME);
        Ok(Self {
            runtime_dir,
            socket,
            lock,
        })
    }

    /// The owner-private runtime directory.
    #[must_use]
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// The endpoint socket path.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The single-instance lock file path.
    #[must_use]
    pub fn lock(&self) -> &Path {
        &self.lock
    }
}

/// Identity of a filesystem object, used to prove we still own what we bound.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    dev: u64,
    ino: u64,
    uid: u32,
    mode: u32,
}

#[cfg(unix)]
impl SocketIdentity {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            uid: metadata.uid(),
            mode: metadata.mode() & 0o777,
        }
    }
}

/// A bound, locked, attested endpoint.
///
/// Dropping the value removes the socket only when the path still names the
/// object this process bound; [`BoundEndpoint::close`] reports why a removal
/// was refused instead of hiding it. The single-instance lock is released
/// after the socket removal (including at process exit or crash, where the
/// kernel releases it).
#[cfg(unix)]
pub struct BoundEndpoint {
    listener: UnixListener,
    paths: EndpointPaths,
    identity: SocketIdentity,
    uid: u32,
    closed: bool,
    _lock: Flock<File>,
}

#[cfg(unix)]
impl fmt::Debug for BoundEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundEndpoint")
            .field("socket", &self.paths.socket)
            .field("runtime_dir", &self.paths.runtime_dir)
            .field("uid", &self.uid)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

#[cfg(unix)]
impl BoundEndpoint {
    /// The bound listening socket, for the service loop (bitty#2607).
    #[must_use]
    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }

    /// The endpoint socket path.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.paths.socket
    }

    /// The owner-private runtime directory.
    #[must_use]
    pub fn runtime_dir(&self) -> &Path {
        &self.paths.runtime_dir
    }

    /// Re-run the directory and socket attestation for the bounded tamper
    /// interval the service loop owns.
    ///
    /// # Errors
    ///
    /// [`EndpointError::Tampered`] when the runtime directory or socket is
    /// missing, replaced, a symlink, the wrong type, owned by another uid, or
    /// has drifted from the contract mode.
    pub fn reattest(&self) -> Result<(), EndpointError> {
        attest_directory_path(&self.paths.runtime_dir, self.uid)?;
        let metadata = symlink_metadata(&self.paths.socket, "inspect the endpoint socket")?;
        attest_socket_metadata(&self.paths.socket, &metadata, self.uid)?;
        if SocketIdentity::from_metadata(&metadata) != self.identity {
            return Err(EndpointError::Tampered {
                path: self.paths.socket.clone(),
                kind: TamperKind::Replaced,
            });
        }
        Ok(())
    }

    /// Close the endpoint and remove its socket when this process still owns
    /// the path.
    ///
    /// # Errors
    ///
    /// [`EndpointError::Tampered`] when the path no longer names the bound
    /// socket; nothing is unlinked in that case. Filesystem failures are
    /// reported as [`EndpointError::Io`].
    pub fn close(mut self) -> Result<(), EndpointError> {
        self.closed = true;
        self.remove_owned_socket()
    }

    fn remove_owned_socket(&self) -> Result<(), EndpointError> {
        let path = &self.paths.socket;
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(EndpointError::Io {
                    action: "inspect the endpoint socket during shutdown",
                    path: path.clone(),
                    source,
                });
            }
        };
        if metadata.file_type().is_symlink() {
            return Err(EndpointError::Tampered {
                path: path.clone(),
                kind: TamperKind::Symlink,
            });
        }
        let observed = SocketIdentity::from_metadata(&metadata);
        let same_object = observed.dev == self.identity.dev
            && observed.ino == self.identity.ino
            && observed.uid == self.identity.uid;
        if !same_object {
            return Err(EndpointError::Tampered {
                path: path.clone(),
                kind: TamperKind::Replaced,
            });
        }
        std::fs::remove_file(path).map_err(|source| EndpointError::Io {
            action: "remove the endpoint socket during shutdown",
            path: path.clone(),
            source,
        })
    }
}

#[cfg(unix)]
impl Drop for BoundEndpoint {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.remove_owned_socket();
        }
    }
}

/// The stub endpoint value on platforms without an accepted transport.
///
/// [`open_endpoint`] never returns this value: every non-Unix target refuses
/// with [`EndpointError::UnsupportedPlatform`] before binding.
#[cfg(not(unix))]
#[derive(Debug)]
pub struct BoundEndpoint {
    _private: (),
}

/// Resolve, lock, attest, and bind the per-user endpoint.
///
/// # Errors
///
/// - [`EndpointError::UnsupportedPlatform`] on a cell without an accepted
///   transport; nothing is created.
/// - [`EndpointError::NoEndpoint`] / [`EndpointError::InvalidPath`] /
///   [`EndpointError::PathTooLong`] for an unusable endpoint path.
/// - [`EndpointError::AlreadyRunning`] when another instance holds the lock or
///   a live endpoint answers at the path.
/// - [`EndpointError::Tampered`] for a symlink, non-socket, foreign-owned, or
///   wrong-mode endpoint; the offending object is left in place.
/// - [`EndpointError::StaleUnverifiable`] when an existing endpoint cannot be
///   proven stale; takeover is refused.
/// - [`EndpointError::Io`] for filesystem failures.
pub fn open_endpoint(env: &EndpointEnv) -> Result<BoundEndpoint, EndpointError> {
    open_endpoint_for_cell(PlatformCell::current(), env)
}

fn open_endpoint_for_cell(
    cell: PlatformCell,
    env: &EndpointEnv,
) -> Result<BoundEndpoint, EndpointError> {
    match cell.transport() {
        EndpointTransport::UnixSocket => open_unix(env),
        EndpointTransport::WindowsNamedPipe | EndpointTransport::Unsupported => {
            Err(EndpointError::UnsupportedPlatform { cell })
        }
    }
}

#[cfg(unix)]
fn open_unix(env: &EndpointEnv) -> Result<BoundEndpoint, EndpointError> {
    let paths = EndpointPaths::resolve(env)?;
    let uid = getuid().as_raw();
    prepare_runtime_dir(&paths.runtime_dir, uid)?;
    let lock = acquire_instance_lock(&paths, uid)?;
    clear_stale_socket(&paths.socket, uid)?;
    let (listener, identity) = bind_endpoint(&paths.socket, uid)?;
    let bound = BoundEndpoint {
        listener,
        paths: paths.clone(),
        identity,
        uid,
        closed: false,
        _lock: lock,
    };
    bound.reattest()?;
    Ok(bound)
}

#[cfg(not(unix))]
fn open_unix(_env: &EndpointEnv) -> Result<BoundEndpoint, EndpointError> {
    Err(EndpointError::UnsupportedPlatform {
        cell: PlatformCell::current(),
    })
}

#[cfg(unix)]
fn prepare_runtime_dir(dir: &Path, uid: u32) -> Result<(), EndpointError> {
    if let Some(parent) = dir.parent() {
        match std::fs::metadata(parent) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(EndpointError::NoEndpoint {
                    reason: format!("the runtime root {} is not a directory", parent.display()),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(EndpointError::NoEndpoint {
                    reason: format!("the runtime root {} does not exist", parent.display()),
                });
            }
            Err(source) => {
                return Err(EndpointError::Io {
                    action: "inspect the runtime root",
                    path: parent.to_path_buf(),
                    source,
                });
            }
        }
    }
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) => attest_directory(dir, &metadata, uid),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let create = std::fs::DirBuilder::new()
                .mode(RUNTIME_DIR_MODE)
                .create(dir);
            if let Err(source) = create {
                if source.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(EndpointError::Io {
                        action: "create the endpoint runtime directory",
                        path: dir.to_path_buf(),
                        source,
                    });
                }
            }
            let metadata = symlink_metadata(dir, "attest the endpoint runtime directory")?;
            attest_directory(dir, &metadata, uid)
        }
        Err(source) => Err(EndpointError::Io {
            action: "inspect the endpoint runtime directory",
            path: dir.to_path_buf(),
            source,
        }),
    }
}

#[cfg(unix)]
fn attest_directory_path(dir: &Path, uid: u32) -> Result<(), EndpointError> {
    let metadata = symlink_metadata(dir, "attest the endpoint runtime directory")?;
    attest_directory(dir, &metadata, uid)
}

#[cfg(unix)]
fn attest_directory(path: &Path, metadata: &Metadata, uid: u32) -> Result<(), EndpointError> {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(tampered(path, TamperKind::Symlink));
    }
    if !file_type.is_dir() {
        return Err(tampered(path, TamperKind::NotADirectory));
    }
    attest_owner_mode(
        path,
        metadata.uid(),
        metadata.mode() & 0o777,
        uid,
        RUNTIME_DIR_MODE,
    )
}

/// Attest the type and owner of an existing endpoint path. The mode is
/// deliberately not checked here: a dead socket left by a crash between
/// `bind` and `chmod` is still this user's object to reclaim, while a live
/// endpoint is never unlinked and its own tamper interval judges its mode.
#[cfg(unix)]
fn attest_existing_socket(path: &Path, metadata: &Metadata, uid: u32) -> Result<(), EndpointError> {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(tampered(path, TamperKind::Symlink));
    }
    if !file_type.is_socket() {
        return Err(tampered(path, TamperKind::NotASocket));
    }
    if metadata.uid() != uid {
        return Err(tampered(
            path,
            TamperKind::ForeignOwner {
                owner: metadata.uid(),
                expected: uid,
            },
        ));
    }
    Ok(())
}

/// Attest a bound endpoint socket: type, owner, and the contract `0600` mode.
#[cfg(unix)]
fn attest_socket_metadata(path: &Path, metadata: &Metadata, uid: u32) -> Result<(), EndpointError> {
    attest_existing_socket(path, metadata, uid)?;
    attest_owner_mode(
        path,
        metadata.uid(),
        metadata.mode() & 0o777,
        uid,
        SOCKET_MODE,
    )
}

#[cfg(unix)]
fn attest_owner_mode(
    path: &Path,
    owner: u32,
    mode: u32,
    expected_owner: u32,
    expected_mode: u32,
) -> Result<(), EndpointError> {
    if owner != expected_owner {
        return Err(tampered(
            path,
            TamperKind::ForeignOwner {
                owner,
                expected: expected_owner,
            },
        ));
    }
    if mode != expected_mode {
        return Err(tampered(
            path,
            TamperKind::WrongMode {
                mode,
                expected: expected_mode,
            },
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn tampered(path: &Path, kind: TamperKind) -> EndpointError {
    EndpointError::Tampered {
        path: path.to_path_buf(),
        kind,
    }
}

#[cfg(unix)]
fn symlink_metadata(path: &Path, action: &'static str) -> Result<Metadata, EndpointError> {
    std::fs::symlink_metadata(path).map_err(|source| EndpointError::Io {
        action,
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(unix)]
fn acquire_instance_lock(paths: &EndpointPaths, uid: u32) -> Result<Flock<File>, EndpointError> {
    let lock_path = &paths.lock;
    match std::fs::symlink_metadata(lock_path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(tampered(lock_path, TamperKind::Symlink));
            }
            if !metadata.file_type().is_file() {
                return Err(tampered(lock_path, TamperKind::NotARegularFile));
            }
            attest_owner_mode(
                lock_path,
                metadata.uid(),
                metadata.mode() & 0o777,
                uid,
                LOCK_FILE_MODE,
            )?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(EndpointError::Io {
                action: "inspect the single-instance lock file",
                path: lock_path.clone(),
                source,
            });
        }
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(LOCK_FILE_MODE)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(lock_path)
        .map_err(|source| {
            if source.raw_os_error() == Some(nix::libc::ELOOP) {
                tampered(lock_path, TamperKind::Symlink)
            } else {
                EndpointError::Io {
                    action: "open the single-instance lock file",
                    path: lock_path.clone(),
                    source,
                }
            }
        })?;
    let metadata = file.metadata().map_err(|source| EndpointError::Io {
        action: "attest the single-instance lock file",
        path: lock_path.clone(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(tampered(lock_path, TamperKind::NotARegularFile));
    }
    attest_owner_mode(
        lock_path,
        metadata.uid(),
        metadata.mode() & 0o777,
        uid,
        LOCK_FILE_MODE,
    )?;

    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(lock),
        Err((_file, Errno::EWOULDBLOCK)) => Err(EndpointError::AlreadyRunning {
            socket: paths.socket.clone(),
            lock: lock_path.clone(),
        }),
        Err((_file, errno)) => Err(EndpointError::Io {
            action: "take the single-instance lock",
            path: lock_path.clone(),
            source: std::io::Error::from_raw_os_error(errno as i32),
        }),
    }
}

/// Bind the socket, apply `0600`, and attest the resulting path.
///
/// A Linux socket inode ignores `fchmod`, so the mode is applied through the
/// path. A successful `bind` is what proves the path is ours: `bind` creates
/// the file and fails with `EADDRINUSE` when one already exists. The path is
/// re-attested after the `chmod` (no symlink, a socket, this uid, `0600`) and
/// its `(device, inode)` is recorded for shutdown and the tamper interval.
/// The runtime directory is `0700` and owned by this uid, so only a same-uid
/// process could race the path — and same-uid is already inside the daemon's
/// trust boundary.
#[cfg(unix)]
fn bind_endpoint(path: &Path, uid: u32) -> Result<(UnixListener, SocketIdentity), EndpointError> {
    let listener = bind_socket(path)?;
    let result = bind_endpoint_inner(path, uid);
    if result.is_err() {
        remove_socket_if_owned(path, uid);
    }
    result.map(|identity| (listener, identity))
}

#[cfg(unix)]
fn bind_endpoint_inner(path: &Path, uid: u32) -> Result<SocketIdentity, EndpointError> {
    let before = symlink_metadata(path, "inspect the newly bound endpoint socket")?;
    if !before.file_type().is_socket() {
        return Err(tampered(path, TamperKind::NotASocket));
    }
    std::fs::set_permissions(path, Permissions::from_mode(SOCKET_MODE)).map_err(|source| {
        EndpointError::Io {
            action: "set the endpoint socket mode",
            path: path.to_path_buf(),
            source,
        }
    })?;
    let after = symlink_metadata(path, "attest the newly bound endpoint socket")?;
    attest_socket_metadata(path, &after, uid)?;
    Ok(SocketIdentity::from_metadata(&after))
}

/// Remove a partially bound socket after a failed startup. Only a socket
/// owned by this uid is removed.
#[cfg(unix)]
fn remove_socket_if_owned(path: &Path, uid: u32) {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_socket() && metadata.uid() == uid {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(unix)]
fn bind_socket(path: &Path) -> Result<UnixListener, EndpointError> {
    UnixListener::bind(path).map_err(|source| match source.kind() {
        std::io::ErrorKind::AddrInUse => EndpointError::AlreadyRunning {
            socket: path.to_path_buf(),
            lock: path.with_file_name(LOCK_FILE_NAME),
        },
        _ => EndpointError::Io {
            action: "bind the endpoint socket",
            path: path.to_path_buf(),
            source,
        },
    })
}

/// Result of probing an existing endpoint path.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// A listener accepted the connection: never unlink.
    Live,
    /// Nothing listens on the path.
    Stale,
    /// The probe could not classify the path; refuse takeover.
    Ambiguous,
}

#[cfg(unix)]
fn clear_stale_socket(path: &Path, uid: u32) -> Result<(), EndpointError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(EndpointError::Io {
                action: "inspect the existing endpoint path",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    attest_existing_socket(path, &metadata, uid)?;
    let identity = SocketIdentity::from_metadata(&metadata);
    match probe_endpoint(path) {
        Probe::Live => Err(EndpointError::AlreadyRunning {
            socket: path.to_path_buf(),
            lock: path.with_file_name(LOCK_FILE_NAME),
        }),
        Probe::Stale => reclaim_stale_socket(path, identity),
        Probe::Ambiguous => Err(tampered(path, TamperKind::ProbeAmbiguous)),
    }
}

/// Probe an existing endpoint without blocking and without sending data.
///
/// Unix-domain `connect` completes synchronously: a live listener accepts,
/// a dead socket file returns `ECONNREFUSED`, and a live listener with a full
/// accept queue returns `EAGAIN` (treated as live: never unlink).
#[cfg(unix)]
fn probe_endpoint(path: &Path) -> Probe {
    let Ok(address) = UnixAddr::new(path) else {
        return Probe::Ambiguous;
    };
    let Ok(fd) = nonblocking_stream_socket() else {
        return Probe::Ambiguous;
    };
    match connect(fd.as_raw_fd(), &address) {
        Ok(()) => Probe::Live,
        Err(Errno::ECONNREFUSED | Errno::ENOENT) => Probe::Stale,
        Err(Errno::EAGAIN | Errno::EINPROGRESS) => Probe::Live,
        Err(_) => Probe::Ambiguous,
    }
}

#[cfg(unix)]
fn nonblocking_stream_socket() -> Result<OwnedFd, Errno> {
    let fd = socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )?;
    fcntl(&fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    fcntl(&fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
    Ok(fd)
}

#[cfg(unix)]
fn reclaim_stale_socket(path: &Path, expected: SocketIdentity) -> Result<(), EndpointError> {
    let quarantine = match quarantine_path(path) {
        Some(candidate) => candidate,
        None => {
            return Err(EndpointError::StaleUnverifiable {
                path: path.to_path_buf(),
                reason: "the endpoint path has no parent directory".to_string(),
            });
        }
    };
    match std::fs::rename(path, &quarantine) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(EndpointError::Io {
                action: "quarantine the stale endpoint socket",
                path: path.to_path_buf(),
                source,
            });
        }
    }

    let metadata = match std::fs::symlink_metadata(&quarantine) {
        Ok(metadata) => metadata,
        Err(source) => {
            return Err(EndpointError::Io {
                action: "inspect the quarantined stale endpoint",
                path: quarantine.clone(),
                source,
            });
        }
    };
    let file_type = metadata.file_type();
    let observed = SocketIdentity::from_metadata(&metadata);
    let refusal = if file_type.is_symlink() {
        Some(TamperKind::Symlink)
    } else if !file_type.is_socket() {
        Some(TamperKind::NotASocket)
    } else if observed != expected {
        Some(TamperKind::Replaced)
    } else {
        None
    };
    if let Some(kind) = refusal {
        let restored = restore_quarantined(&quarantine, path);
        return Err(EndpointError::StaleUnverifiable {
            path: path.to_path_buf(),
            reason: format!("the quarantined object failed attestation ({kind}); {restored}"),
        });
    }

    std::fs::remove_file(&quarantine).map_err(|source| EndpointError::Io {
        action: "remove the quarantined stale endpoint",
        path: quarantine.clone(),
        source,
    })
}

#[cfg(unix)]
fn quarantine_path(path: &Path) -> Option<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent()?;
    let name = path.file_name().map_or_else(
        || "bittyd.sock".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    for _ in 0..8 {
        let candidate = parent.join(format!(
            ".{name}.stale-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        if std::fs::symlink_metadata(&candidate).is_err() {
            return Some(candidate);
        }
    }
    None
}

/// Put a quarantined object back without overwriting anything that appeared
/// at the endpoint path meanwhile, and describe the outcome.
#[cfg(unix)]
fn restore_quarantined(quarantine: &Path, path: &Path) -> String {
    match std::fs::hard_link(quarantine, path) {
        Ok(()) => match std::fs::remove_file(quarantine) {
            Ok(()) => "original restored".to_string(),
            Err(error) => format!(
                "original restored; quarantine {} not removed: {error}",
                quarantine.display()
            ),
        },
        Err(error) => format!(
            "original left at {} because the path could not be restored: {error}",
            quarantine.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn scratch_dir(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};

        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "bittyd-endpoint-unit-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create scratch dir");
        path
    }

    #[test]
    fn unsupported_cells_refuse_without_creating_anything() {
        #[cfg(unix)]
        let root = scratch_dir("unsupported");
        #[cfg(not(unix))]
        let root = std::env::temp_dir();
        let env = EndpointEnv::new(Some(root.clone().into_os_string()), None);

        for cell in [
            PlatformCell::FreeBsdX86_64,
            PlatformCell::WindowsX86_64,
            PlatformCell::Other,
        ] {
            let error =
                open_endpoint_for_cell(cell, &env).expect_err("the cell has no accepted transport");
            assert!(
                matches!(error, EndpointError::UnsupportedPlatform { cell: refused } if refused == cell),
                "unexpected error for {cell}: {error}"
            );
            assert_eq!(error.result_code(), Some("UnsupportedPlatform"));
        }

        #[cfg(unix)]
        {
            let entries = std::fs::read_dir(&root).expect("read scratch dir").count();
            assert_eq!(entries, 0, "a refused cell must not create an endpoint");
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    #[cfg(unix)]
    #[test]
    fn owner_and_mode_are_attested_as_a_pair() {
        let path = Path::new("/endpoint");
        match attest_owner_mode(path, 1000, SOCKET_MODE, 1001, SOCKET_MODE).unwrap_err() {
            EndpointError::Tampered {
                kind: TamperKind::ForeignOwner { owner, expected },
                ..
            } => {
                assert_eq!(owner, 1000);
                assert_eq!(expected, 1001);
            }
            other => panic!("unexpected error: {other}"),
        }
        match attest_owner_mode(path, 1000, 0o644, 1000, SOCKET_MODE).unwrap_err() {
            EndpointError::Tampered {
                kind: TamperKind::WrongMode { mode, expected },
                ..
            } => {
                assert_eq!(mode, 0o644);
                assert_eq!(expected, SOCKET_MODE);
            }
            other => panic!("unexpected error: {other}"),
        }
        assert!(attest_owner_mode(path, 1000, SOCKET_MODE, 1000, SOCKET_MODE).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn endpoint_resolution_requires_a_runtime_root() {
        let error = EndpointPaths::resolve(&EndpointEnv::default()).unwrap_err();
        assert!(matches!(error, EndpointError::NoEndpoint { .. }));
        assert_eq!(error.result_code(), None);

        let relative = EndpointEnv::new(Some(OsString::from("run")), None);
        assert!(matches!(
            EndpointPaths::resolve(&relative).unwrap_err(),
            EndpointError::NoEndpoint { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn endpoint_override_must_be_absolute_and_named() {
        for override_path in ["bittyd.sock", "/"] {
            let env = EndpointEnv::new(None, Some(OsString::from(override_path)));
            assert!(matches!(
                EndpointPaths::resolve(&env).unwrap_err(),
                EndpointError::InvalidPath { .. }
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn over_long_endpoint_is_refused_before_creation() {
        let root = scratch_dir("long");
        let name = "s".repeat(MAX_SOCKET_PATH_BYTES);
        let socket = root.join(name);
        let env = EndpointEnv::new(None, Some(socket.clone().into_os_string()));
        let error = EndpointPaths::resolve(&env).expect_err("path is over the bound");
        match error {
            EndpointError::PathTooLong { bytes, max, .. } => {
                assert_eq!(max, MAX_SOCKET_PATH_BYTES);
                assert!(bytes > max);
            }
            other => panic!("unexpected error: {other}"),
        }
        assert_eq!(std::fs::read_dir(&root).expect("read root").count(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn derived_paths_follow_the_contract_layout() {
        let root = scratch_dir("derived");
        let env = EndpointEnv::new(Some(root.clone().into_os_string()), None);
        let paths = EndpointPaths::resolve(&env).expect("resolve");
        assert_eq!(paths.runtime_dir(), root.join(RUNTIME_SUBDIR));
        assert_eq!(
            paths.socket(),
            root.join(RUNTIME_SUBDIR).join(SOCKET_FILE_NAME)
        );
        assert_eq!(paths.lock(), root.join(RUNTIME_SUBDIR).join(LOCK_FILE_NAME));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn override_paths_are_used_verbatim() {
        let root = scratch_dir("override");
        let socket = root.join("nested.sock");
        let env = EndpointEnv::new(
            Some(OsString::from("/unused")),
            Some(socket.clone().into_os_string()),
        );
        let paths = EndpointPaths::resolve(&env).expect("resolve");
        assert_eq!(paths.socket(), socket);
        assert_eq!(paths.runtime_dir(), root);
        assert_eq!(paths.lock(), root.join(LOCK_FILE_NAME));
        let _ = std::fs::remove_dir_all(&root);
    }
}
