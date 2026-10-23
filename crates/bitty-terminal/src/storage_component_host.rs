//! Opt-in host client for the `bitty-storage` component (bitty#2495, CTX-1479).
//!
//! This module is an architecture seam, not a startup path. The linked
//! `StorageSessionBackend` / `StorageKvBackend` adapters remain the default and
//! this module is not installed by `main`. Enabling `storage-component` is a
//! build-time opt-in only; no CLI flag, configuration key, or first-run
//! migration reaches it.
//!
//! # What the host owns
//!
//! - **Install authority**: the component is resolved through the shared
//!   two-tier layout ([`bitty_runtime::component::resolve_search_for_protocol`]),
//!   which re-reads `current`, validates `bitty-component.toml`, checks the v1
//!   protocol range, and verifies the executable SHA-256 before every spawn.
//! - **Policy authority**: [`StorageComponentGate`] denies safe mode, a missing
//!   capability grant, and the default-off preference **before** any install
//!   resolution or process creation.
//! - **Storage root**: [`StorageRootSelection`] derives one confined root from
//!   an explicit base directory and profile, creates it `0700`, and rejects
//!   symlinked/escaping paths. The component receives exactly one `--root`
//!   argument; no filesystem path ever travels on the wire.
//! - **Process lifetime**: the child is spawned with a cleared environment, in
//!   its own process group, with piped stdio, a bounded stderr ring, a bounded
//!   inbound frame queue, per-exchange deadlines, an explicit handshake
//!   deadline, and a bounded clean shutdown.
//! - **Wire safety**: only protocol-v1 namespace open, paged snapshot read,
//!   batch commit, rollback, and shutdown are spoken. Every response is
//!   validated against the request id and namespace, every bound is enforced
//!   locally, and malformed/oversized/EOF/crash/timeout paths poison the
//!   client and stop the recorded process group.
//! - **Mutation safety**: a commit or rollback is never replayed. The
//!   submission bit is set before the request is written, and any mutation
//!   error poisons the connection so a caller cannot silently retry into a
//!   state the component may already have applied.
//!
//! # Pin state and re-pin
//!
//! `bitty-storage` PR #18 is **not merged**. Its current head
//! `5b5f19eb` is pinned only as the optional `bitty-storage-component`
//! dependency; the linked model stays on `7bf5177`. A follow-up to PR #18 adds
//! `Uncertain = 11` (keeping `Internal = 10`) and adjusts cached-open/eviction
//! behavior, so the host must not be enabled or repinned to a merged SHA yet.
//!
//! After the follow-up lands:
//!
//! 1. Change the `bitty-storage-component` `rev` in
//!    `crates/bitty-terminal/Cargo.toml` to the new PR head (or the merge
//!    commit on `main` once #18 is merged).
//! 2. Run `cargo check -p bitty-terminal --features storage-component` to
//!    refresh `Cargo.lock` for both storage pins.
//! 3. Re-read [`StorageComponentFault::from_wire_code`] and confirm the
//!    reserved `11 = Uncertain` row plus the mutation-poison rule still match
//!    the component. The table is the only error-code mapping in this module.
//! 4. Only then wire the injectable gate into the composition root and add a
//!    user-facing enable. Until that point this module stays default-off.
//!
//! Unknown error codes are always classified as
//! [`StorageComponentFault::Unknown`] (a generic component failure), never as a
//! success or a retryable condition. A code the pinned wire codec itself
//! rejects fails earlier as [`StorageHostError::Protocol`]; both paths stay
//! fail-closed.

#![allow(dead_code)]

use std::fmt;
use std::path::{Path, PathBuf};
#[cfg(not(feature = "storage-component"))]
use std::sync::Arc;
use std::time::Duration;

/// Default per-exchange deadline. Callers may pass a shorter bound.
pub const DEFAULT_OPERATION_DEADLINE: Duration = Duration::from_secs(2);
/// Default `HelloAck` deadline.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Default deadline for the `ShutdownAck`.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
/// Maximum time the host waits for the child to exit after `ShutdownAck`.
pub const SHUTDOWN_EXIT_GRACE: Duration = Duration::from_secs(2);
/// Bound on joining host-owned I/O threads during shutdown.
pub const THREAD_JOIN_TIMEOUT: Duration = Duration::from_millis(250);
/// Bound on the best-effort exit wait in `Drop`.
pub const DROP_EXIT_GRACE: Duration = Duration::from_millis(100);

/// Bounded inbound queue between the stdout reader and the owner.
///
/// A flooding component blocks its reader thread (and therefore its stdout
/// pipe) instead of growing host memory.
pub const INBOUND_QUEUE_FRAMES: usize = 16;

/// Bounded stderr capacity per component instance (newest bytes kept).
pub const STDERR_RING_BYTES: usize = 64 * 1024;

/// Maximum rows accepted from one complete snapshot (protocol v1).
pub const MAX_COMPONENT_ROWS: usize = 4096;
/// Maximum complete opaque object accepted by this adapter (protocol v1).
pub const MAX_COMPONENT_BYTES: usize = 1024 * 1024;
/// Maximum value represented by one component row (protocol v1).
pub const MAX_COMPONENT_VALUE_BYTES: usize = 128 * 1024;
/// Maximum pages accepted for one snapshot. One row per page is the worst
/// legal case; the extra page covers a final empty `done` page.
pub const MAX_COMPONENT_PAGES: usize = MAX_COMPONENT_ROWS + 1;
/// Maximum namespace-name bytes (`[a-z0-9][a-z0-9._-]{0,63}`).
pub const MAX_NAMESPACE_BYTES: usize = 64;
/// Maximum key bytes (`[a-z0-9][a-z0-9._-]{0,127}`).
pub const MAX_KEY_BYTES: usize = 128;
/// Maximum upserts or deletes in one commit batch.
pub const MAX_BATCH_ROWS: usize = 128;
/// Maximum encoded row/delete payload bytes in one commit batch.
pub const MAX_BATCH_BYTES: usize = 240 * 1024;

/// Fixed component name. Never caller-controlled.
pub const COMPONENT_NAME: &str = "storage";
/// Fixed executable base name. Never caller-controlled.
pub const COMPONENT_EXECUTABLE: &str = "bitty-storage";
/// Directory below the explicit state base that holds component roots.
pub const STORAGE_COMPONENT_DIR_NAME: &str = "storage-component";
/// Default profile segment for [`StorageRootSelection::under`].
pub const DEFAULT_PROFILE: &str = "default";
/// Marker content written by the explicit migration helper.
pub const MIGRATION_MARKER: &[u8] = b"migrated\n";
/// Maximum marker file bytes accepted by the migration helper.
pub const MIGRATION_MARKER_MAX_BYTES: usize = 64;

// ── policy gate ─────────────────────────────────────────────────────────────

/// Host policy gate. Every field must allow component use before the host
/// resolves an install, opens a root, or spawns a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageComponentGate {
    /// Explicit architecture preference; false preserves linked behavior.
    pub preferred: bool,
    /// Safe mode denies all component storage access.
    pub safe_mode: bool,
    /// The storage capability has been granted by the owning policy layer.
    pub capability_granted: bool,
}

impl StorageComponentGate {
    /// Default-off policy.
    pub const DISABLED: Self = Self {
        preferred: false,
        safe_mode: false,
        capability_granted: false,
    };

    /// Whether this gate permits component storage.
    #[must_use]
    pub const fn allows(self) -> bool {
        self.preferred && !self.safe_mode && self.capability_granted
    }
}

impl Default for StorageComponentGate {
    fn default() -> Self {
        Self::DISABLED
    }
}

// ── errors ──────────────────────────────────────────────────────────────────

/// Typed component error categories (protocol v1).
///
/// This table is deliberately independent of the pinned crate's enum so a
/// protocol follow-up can add codes without changing host call sites. The only
/// mapping lives in [`Self::from_wire_code`]; unknown codes become
/// [`Self::Unknown`] and are reported as a generic component failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageComponentFault {
    /// The peer violated the protocol.
    Protocol,
    /// A request was malformed or violated a bound.
    InvalidRequest,
    /// A capacity or byte budget is exhausted.
    Budget,
    /// The operation is unsupported on this component/platform.
    Unsupported,
    /// The namespace was never opened on this connection.
    UnknownNamespace,
    /// `expected_revision` does not match the current revision.
    StaleRevision,
    /// Another process holds the namespace lock.
    Locked,
    /// A snapshot or journal is structurally invalid.
    CorruptState,
    /// A filesystem commit or load failed.
    Io,
    /// Internal component failure.
    Internal,
    /// Commit outcome is unknown. Reserved for the pending storage PR #18
    /// follow-up (`Uncertain = 11`); a committed mutation with this result is
    /// never replayed.
    Uncertain,
    /// A code this host does not know. Treated as a generic component failure.
    Unknown,
}

impl StorageComponentFault {
    /// Maps one protocol-v1 error code. Codes not listed here are
    /// [`Self::Unknown`] by construction.
    #[must_use]
    pub const fn from_wire_code(code: u8) -> Self {
        match code {
            1 => Self::Protocol,
            2 => Self::InvalidRequest,
            3 => Self::Budget,
            4 => Self::Unsupported,
            5 => Self::UnknownNamespace,
            6 => Self::StaleRevision,
            7 => Self::Locked,
            8 => Self::CorruptState,
            9 => Self::Io,
            10 => Self::Internal,
            // Reserved by the pending storage PR #18 follow-up. Confirm this
            // row when the new head is re-pinned (see the module docs).
            11 => Self::Uncertain,
            _ => Self::Unknown,
        }
    }

    /// Stable content-free label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Protocol => "protocol",
            Self::InvalidRequest => "invalid-request",
            Self::Budget => "budget",
            Self::Unsupported => "unsupported",
            Self::UnknownNamespace => "unknown-namespace",
            Self::StaleRevision => "stale-revision",
            Self::Locked => "locked",
            Self::CorruptState => "corrupt-state",
            Self::Io => "io",
            Self::Internal => "internal",
            Self::Uncertain => "uncertain",
            Self::Unknown => "unknown",
        }
    }
}

/// Bounded host-client failures. Diagnostics never include paths, keys, or
/// values from the component protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageHostError {
    /// The optional gate denied component use.
    Denied,
    /// The component was not installed or its descriptor was not verified.
    Resolution,
    /// The child could not be spawned or its pipes failed.
    Spawn,
    /// The peer violated protocol v1 or returned an unexpected message.
    Protocol,
    /// The child exited or closed stdout unexpectedly.
    Eof,
    /// An operation exceeded its explicit deadline.
    Timeout,
    /// The component returned a typed error.
    Component(StorageComponentFault),
    /// A local bound or namespace rule was violated.
    Bounds,
    /// The client was poisoned after an in-flight failure.
    Poisoned,
    /// A local path escaped its explicit confined root.
    Confinement,
    /// A host-side filesystem operation failed.
    Io,
}

impl fmt::Display for StorageHostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Denied => f.write_str("storage component denied"),
            Self::Resolution => f.write_str("storage component resolution failed"),
            Self::Spawn => f.write_str("storage component spawn failed"),
            Self::Protocol => f.write_str("storage component protocol failure"),
            Self::Eof => f.write_str("storage component closed unexpectedly"),
            Self::Timeout => f.write_str("storage component deadline exceeded"),
            Self::Component(fault) => {
                write!(
                    f,
                    "storage component rejected the request ({})",
                    fault.label()
                )
            }
            Self::Bounds => f.write_str("storage component bounds exceeded"),
            Self::Poisoned => f.write_str("storage component client is unavailable"),
            Self::Confinement => f.write_str("storage component path escaped its confined root"),
            Self::Io => f.write_str("storage component host i/o failed"),
        }
    }
}

impl std::error::Error for StorageHostError {}

// ── accounting and values ───────────────────────────────────────────────────

/// Crash/EOF/timeout/error accounting for the selected component instance.
#[derive(Debug, Default)]
pub struct StorageHostAccounting {
    /// Number of unexpected child exits observed by the host.
    pub crashes: std::sync::atomic::AtomicU64,
    /// Number of stdout EOFs observed while serving requests.
    pub eof: std::sync::atomic::AtomicU64,
    /// Number of operation deadlines that expired.
    pub timeouts: std::sync::atomic::AtomicU64,
    /// Number of protocol violations observed.
    pub protocol_faults: std::sync::atomic::AtomicU64,
    /// Number of typed component errors observed.
    pub component_errors: std::sync::atomic::AtomicU64,
    /// Number of local bound violations observed.
    pub bounds_faults: std::sync::atomic::AtomicU64,
}

/// A row returned by the component, converted without changing wire fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageRow {
    /// Bounded key.
    pub key: String,
    /// Bounded opaque bytes.
    pub value: Vec<u8>,
}

/// Commit result exposed by the host seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageCommit {
    /// New monotonic component revision.
    pub revision: u64,
    /// Whether the component accepted the mutation.
    pub submitted: bool,
}

/// Outcome of an explicit linked-file migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageMigrationOutcome {
    /// The source was read and committed into the component namespace.
    Migrated {
        /// Number of opaque bytes migrated.
        bytes: usize,
    },
    /// The marker exists; nothing was read or written.
    AlreadyMigrated,
    /// The linked source file is absent.
    NoSource,
}

// ── explicit storage root selection ─────────────────────────────────────────

/// Explicit, confined component storage root.
///
/// The root is derived from an absolute base (for example an injected
/// `$XDG_STATE_HOME`), one fixed application directory, [`STORAGE_COMPONENT_DIR_NAME`],
/// and a validated profile. The linked XDG session/store paths never change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageRootSelection {
    base: PathBuf,
    profile: String,
    lexical_root: PathBuf,
}

impl StorageRootSelection {
    /// Selects `<base>/<app>/<storage-component>/<profile>`.
    ///
    /// # Errors
    ///
    /// Returns [`StorageHostError::Confinement`] when the base is not absolute
    /// or the profile is not a valid single path segment.
    pub fn under(
        base: impl Into<PathBuf>,
        profile: impl Into<String>,
    ) -> Result<Self, StorageHostError> {
        let base = base.into();
        let profile = profile.into();
        if !base.is_absolute() || !valid_name(&profile, MAX_NAMESPACE_BYTES) {
            return Err(StorageHostError::Confinement);
        }
        let lexical_root = base
            .join(bitty_runtime::SESSION_APP_DIR_NAME)
            .join(STORAGE_COMPONENT_DIR_NAME)
            .join(&profile);
        Ok(Self {
            base,
            profile,
            lexical_root,
        })
    }

    /// Selected profile segment.
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Lexical root path (not yet prepared or canonicalized).
    #[must_use]
    pub fn lexical_root(&self) -> &Path {
        &self.lexical_root
    }

    /// Creates the root if needed, verifies it stays below the base, refuses a
    /// final symlink, and restricts permissions to the owner on Unix.
    ///
    /// # Errors
    ///
    /// Returns [`StorageHostError::Confinement`] when the base/root cannot be
    /// canonicalized, the canonical root is outside the canonical base, or the
    /// final component is a symlink or non-directory.
    pub fn prepare(&self) -> Result<PathBuf, StorageHostError> {
        let base = std::fs::canonicalize(&self.base).map_err(|_| StorageHostError::Confinement)?;
        if !base.is_dir() {
            return Err(StorageHostError::Confinement);
        }
        reject_symlink_ancestors(&base, &self.lexical_root)?;
        std::fs::create_dir_all(&self.lexical_root).map_err(|_| StorageHostError::Confinement)?;
        let root =
            std::fs::canonicalize(&self.lexical_root).map_err(|_| StorageHostError::Confinement)?;
        if !root.starts_with(&base) {
            return Err(StorageHostError::Confinement);
        }
        let metadata =
            std::fs::symlink_metadata(&root).map_err(|_| StorageHostError::Confinement)?;
        if !metadata.file_type().is_dir() {
            return Err(StorageHostError::Confinement);
        }
        restrict_to_owner(&root)?;
        Ok(root)
    }
}

/// Rejects an existing symlink or non-directory on the lexical root path so
/// `create_dir_all` can never follow a component to an out-of-root target.
fn reject_symlink_ancestors(base: &Path, lexical_root: &Path) -> Result<(), StorageHostError> {
    let relative = lexical_root
        .strip_prefix(base)
        .map_err(|_| StorageHostError::Confinement)?;
    let mut current = base.to_path_buf();
    for part in relative.components() {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                    return Err(StorageHostError::Confinement);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return Err(StorageHostError::Confinement),
        }
    }
    Ok(())
}

/// Host configuration for one component connection.
#[derive(Debug, Clone)]
pub struct StorageComponentConfig {
    /// User-tier component root (`<data_home>/bitty/extensions` in production).
    pub user_root: Option<PathBuf>,
    /// System-tier component root (`/usr/lib/bitty/extensions` in production).
    pub system_root: Option<PathBuf>,
    /// Explicit confined root passed to the child as `--root`.
    pub storage_root: StorageRootSelection,
    /// Injectable policy gate (default-off).
    pub gate: StorageComponentGate,
    /// Per-operation deadline.
    pub operation_deadline: Duration,
    /// `HelloAck` deadline.
    pub handshake_timeout: Duration,
    /// `ShutdownAck` deadline.
    pub shutdown_timeout: Duration,
}

impl StorageComponentConfig {
    /// Default-off configuration with the documented deadlines.
    #[must_use]
    pub fn disabled(storage_root: StorageRootSelection) -> Self {
        Self {
            user_root: None,
            system_root: None,
            storage_root,
            gate: StorageComponentGate::DISABLED,
            operation_deadline: DEFAULT_OPERATION_DEADLINE,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            shutdown_timeout: SHUTDOWN_TIMEOUT,
        }
    }
}

// ── host-side persistence mapping (feature-gated) ───────────────────────────

#[cfg(feature = "storage-component")]
mod enabled {
    use std::fmt;
    use std::io::{self, Read};
    use std::path::{Path, PathBuf};
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use bitty_runtime::component::{StderrRing, resolve_search_for_protocol, stderr_log_tail};
    use bitty_runtime::plugin_runtime::store::{
        KvCommitBackend, KvCommitError, STORE_FILE_MAX_BYTES,
    };
    use bitty_runtime::{
        MAX_SESSION_FILE_BYTES, SessionError, SessionFileBackend, SessionSnapshot,
    };
    use bitty_storage_component::component::protocol::{
        self, ErrorKind, FrameReader, Message, Row, WireError,
    };

    use super::{
        COMPONENT_EXECUTABLE, COMPONENT_NAME, DROP_EXIT_GRACE, INBOUND_QUEUE_FRAMES,
        MAX_BATCH_BYTES, MAX_BATCH_ROWS, MAX_COMPONENT_BYTES, MAX_COMPONENT_PAGES,
        MAX_COMPONENT_ROWS, MAX_COMPONENT_VALUE_BYTES, MAX_KEY_BYTES, MAX_NAMESPACE_BYTES,
        MIGRATION_MARKER, MIGRATION_MARKER_MAX_BYTES, SHUTDOWN_EXIT_GRACE, STDERR_RING_BYTES,
        StorageCommit, StorageComponentConfig, StorageComponentFault, StorageHostAccounting,
        StorageHostError, StorageMigrationOutcome, StorageRow, THREAD_JOIN_TIMEOUT, valid_name,
    };
    use crate::storage_backends::StorageSessionBackend;

    // The local bounds mirror protocol v1; fail the build if the pinned crate
    // disagrees instead of silently drifting.
    const _: () = assert!(MAX_COMPONENT_ROWS == protocol::MAX_ROWS);
    const _: () = assert!(MAX_COMPONENT_BYTES == protocol::MAX_SNAPSHOT_BYTES);
    const _: () = assert!(MAX_COMPONENT_VALUE_BYTES == protocol::MAX_VALUE_BYTES);
    const _: () = assert!(MAX_NAMESPACE_BYTES == protocol::MAX_NAMESPACE_NAME_BYTES);
    const _: () = assert!(MAX_KEY_BYTES == protocol::MAX_KEY_BYTES);
    const _: () = assert!(MAX_BATCH_ROWS == protocol::MAX_BATCH_ROWS);
    const _: () = assert!(MAX_BATCH_BYTES == protocol::MAX_BATCH_BYTES);

    /// One decoded inbound message or a terminal reader error.
    type Inbound = Result<Message, WireError>;

    struct ResponseReader {
        rx: Receiver<Inbound>,
        thread: Option<JoinHandle<()>>,
    }

    impl ResponseReader {
        fn start(stdout: impl Read + Send + 'static) -> io::Result<Self> {
            let (tx, rx) = sync_channel(INBOUND_QUEUE_FRAMES);
            let thread = thread::Builder::new()
                .name("bitty-storage-reader".to_owned())
                .spawn(move || read_loop(stdout, tx))?;
            Ok(Self {
                rx,
                thread: Some(thread),
            })
        }

        fn join_bounded(&mut self) {
            join_handle_bounded(&mut self.thread);
        }
    }

    fn read_loop(stdout: impl Read, tx: SyncSender<Inbound>) {
        let mut reader = FrameReader::new(stdout);
        loop {
            match reader.read_message() {
                Ok(Some(message)) => {
                    if tx.send(Ok(message)).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = tx.send(Err(WireError::Io(io::ErrorKind::UnexpectedEof)));
                    break;
                }
                Err(error) => {
                    let _ = tx.send(Err(error));
                    break;
                }
            }
        }
    }

    struct Inner {
        child: Child,
        stdin: Option<ChildStdin>,
        reader: ResponseReader,
        stderr: Arc<Mutex<StderrRing>>,
        stderr_thread: Option<JoinHandle<()>>,
        next_request: u64,
        poisoned: bool,
        shutdown_requested: bool,
        pid: u32,
    }

    /// Connected protocol-v1 host client.
    pub struct StorageComponentClient {
        inner: Mutex<Inner>,
        accounting: Arc<StorageHostAccounting>,
        submitted: AtomicBool,
        deadline: Duration,
        pid: u32,
    }

    impl fmt::Debug for StorageComponentClient {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("StorageComponentClient")
                .field("pid", &self.pid)
                .field("accounting", &self.accounting)
                .finish_non_exhaustive()
        }
    }

    impl StorageComponentClient {
        /// Resolve, verify, spawn, and complete the protocol-v1 handshake.
        ///
        /// # Errors
        ///
        /// Returns [`StorageHostError::Denied`] before any resolution when the
        /// gate does not allow component storage, and the fail-closed
        /// resolution/spawn/protocol errors otherwise.
        pub fn connect(config: &StorageComponentConfig) -> Result<Arc<Self>, StorageHostError> {
            if !config.gate.allows() {
                return Err(StorageHostError::Denied);
            }
            if config
                .user_root
                .as_ref()
                .is_some_and(|root| !root.is_absolute())
                || config
                    .system_root
                    .as_ref()
                    .is_some_and(|root| !root.is_absolute())
            {
                return Err(StorageHostError::Resolution);
            }
            let searched = resolve_search_for_protocol(
                config.user_root.as_deref(),
                config.system_root.as_deref(),
                COMPONENT_NAME,
                protocol::PROTOCOL_VERSION,
            )
            .map_err(|_| StorageHostError::Resolution)?;
            let resolved = searched.resolved;
            if resolved.descriptor.name != COMPONENT_NAME
                || resolved.descriptor.executable != COMPONENT_EXECUTABLE
            {
                return Err(StorageHostError::Resolution);
            }
            let version_dir = std::fs::canonicalize(&resolved.version_dir)
                .map_err(|_| StorageHostError::Resolution)?;
            let executable = std::fs::canonicalize(&resolved.executable_path)
                .map_err(|_| StorageHostError::Resolution)?;
            if !executable.starts_with(&version_dir) {
                return Err(StorageHostError::Resolution);
            }
            let metadata =
                std::fs::symlink_metadata(&executable).map_err(|_| StorageHostError::Resolution)?;
            if !metadata.file_type().is_file() {
                return Err(StorageHostError::Resolution);
            }
            let root = config.storage_root.prepare()?;

            let mut command = Command::new(&executable);
            command
                .arg("--root")
                .arg(&root)
                .current_dir(&version_dir)
                .env_clear()
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            isolate_process_group(&mut command);

            let mut child = command.spawn().map_err(|_| StorageHostError::Spawn)?;
            let pid = child.id();
            let stdin = match child.stdin.take() {
                Some(stdin) => stdin,
                None => {
                    stop_child(&mut child, pid);
                    return Err(StorageHostError::Spawn);
                }
            };
            let stdout = match child.stdout.take() {
                Some(stdout) => stdout,
                None => {
                    stop_child(&mut child, pid);
                    return Err(StorageHostError::Spawn);
                }
            };
            let stderr = match child.stderr.take() {
                Some(stderr) => stderr,
                None => {
                    stop_child(&mut child, pid);
                    return Err(StorageHostError::Spawn);
                }
            };
            let reader = match ResponseReader::start(stdout) {
                Ok(reader) => reader,
                Err(_) => {
                    stop_child(&mut child, pid);
                    return Err(StorageHostError::Spawn);
                }
            };
            let ring = Arc::new(Mutex::new(StderrRing::new(STDERR_RING_BYTES)));
            let stderr_thread = match spawn_stderr_reader(stderr, Arc::clone(&ring)) {
                Ok(thread) => thread,
                Err(_) => {
                    stop_child(&mut child, pid);
                    return Err(StorageHostError::Spawn);
                }
            };
            let client = Arc::new(Self {
                inner: Mutex::new(Inner {
                    child,
                    stdin: Some(stdin),
                    reader,
                    stderr: ring,
                    stderr_thread: Some(stderr_thread),
                    next_request: 1,
                    poisoned: false,
                    shutdown_requested: false,
                    pid,
                }),
                accounting: Arc::new(StorageHostAccounting::default()),
                submitted: AtomicBool::new(false),
                deadline: config.operation_deadline,
                pid,
            });
            match client.handshake(&resolved.descriptor.version, config.handshake_timeout) {
                Ok(()) => Ok(client),
                Err(error) => {
                    client.abort();
                    Err(error)
                }
            }
        }

        /// Per-operation deadline supplied by the configuration.
        #[must_use]
        pub const fn deadline(&self) -> Duration {
            self.deadline
        }

        /// Recorded child process id (diagnostics only).
        #[must_use]
        pub const fn pid(&self) -> u32 {
            self.pid
        }

        /// Shared accounting counters.
        #[must_use]
        pub fn accounting(&self) -> &Arc<StorageHostAccounting> {
            &self.accounting
        }

        /// Whether a commit or rollback request crossed the host/component
        /// boundary. Callers use this to report an uncertain mutation instead
        /// of replaying it.
        #[must_use]
        pub fn commit_was_submitted(&self) -> bool {
            self.submitted.load(Ordering::Acquire)
        }

        /// Newest bounded stderr bytes, escaped for diagnostics.
        #[must_use]
        pub fn stderr_tail(&self) -> String {
            let ring = match self.inner.lock() {
                Ok(inner) => Arc::clone(&inner.stderr),
                Err(poisoned) => Arc::clone(&poisoned.into_inner().stderr),
            };
            let ring = ring.lock().unwrap_or_else(PoisonError::into_inner);
            stderr_log_tail(&ring.snapshot())
        }

        /// Whether the child is still serving requests.
        #[must_use]
        pub fn is_running(&self) -> bool {
            match self.inner.lock() {
                Ok(inner) => !inner.poisoned && !inner.shutdown_requested,
                Err(_) => false,
            }
        }

        fn handshake(
            &self,
            expected_version: &str,
            deadline: Duration,
        ) -> Result<(), StorageHostError> {
            let hello = Message::Hello {
                min: protocol::PROTOCOL_VERSION,
                max: protocol::PROTOCOL_VERSION,
                component: COMPONENT_NAME.to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            };
            match self.exchange(hello, deadline, false)? {
                Message::HelloAck {
                    protocol: version,
                    component,
                    version: component_version,
                } if version == protocol::PROTOCOL_VERSION
                    && component == COMPONENT_NAME
                    && component_version == expected_version =>
                {
                    Ok(())
                }
                _ => Err(self.fail_closed(StorageHostError::Protocol)),
            }
        }

        /// Open and lock a namespace.
        ///
        /// # Errors
        ///
        /// Returns [`StorageHostError::Bounds`] for an invalid namespace and
        /// the fail-closed protocol/component errors otherwise.
        pub fn open_namespace(
            &self,
            namespace: &str,
            deadline: Duration,
        ) -> Result<u64, StorageHostError> {
            validate_namespace(namespace)?;
            let request = self.next_request()?;
            let response = self.exchange(
                Message::OpenNamespace {
                    request,
                    namespace: namespace.to_owned(),
                },
                deadline,
                false,
            )?;
            match response {
                Message::NamespaceOpened {
                    request: got,
                    namespace: got_name,
                    revision,
                    rows,
                    bytes,
                } if got == request && got_name == namespace => {
                    if rows as usize > MAX_COMPONENT_ROWS || bytes > MAX_COMPONENT_BYTES as u64 {
                        return Err(self.fail_closed(StorageHostError::Bounds));
                    }
                    Ok(revision)
                }
                Message::Error { kind, .. } => Err(self.component_error(kind)),
                _ => Err(self.fail_closed(StorageHostError::Protocol)),
            }
        }

        /// Read a complete bounded snapshot using protocol pages.
        ///
        /// # Errors
        ///
        /// Returns [`StorageHostError::Bounds`] when the component reports or
        /// returns more than the local snapshot/value ceilings, and
        /// [`StorageHostError::Protocol`] for revision/total/start drift,
        /// empty non-final pages, or an unexpected message.
        pub fn read_snapshot(
            &self,
            namespace: &str,
            deadline: Duration,
        ) -> Result<(u64, Vec<StorageRow>), StorageHostError> {
            validate_namespace(namespace)?;
            let mut start = 0u32;
            let mut revision = None;
            let mut total_rows = None;
            let mut total_bytes = None;
            let mut rows = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            let mut pages = 0usize;
            loop {
                pages += 1;
                if pages > MAX_COMPONENT_PAGES {
                    return Err(self.fail_closed(StorageHostError::Protocol));
                }
                let request = self.next_request()?;
                let response = self.exchange(
                    Message::ReadSnapshot {
                        request,
                        namespace: namespace.to_owned(),
                        start_row: start,
                        max_rows: protocol::MAX_READ_ROWS as u32,
                        max_bytes: protocol::MAX_READ_BYTES as u32,
                    },
                    deadline,
                    false,
                )?;
                match response {
                    Message::SnapshotChunk {
                        request: got,
                        namespace: got_name,
                        revision: got_revision,
                        start_row,
                        rows: page,
                        done,
                        total_rows: got_total_rows,
                        total_bytes: got_total_bytes,
                    } if got == request => {
                        if got_name != namespace || start_row != start {
                            return Err(self.fail_closed(StorageHostError::Protocol));
                        }
                        if page.len() > protocol::MAX_READ_ROWS
                            || got_total_rows as usize > MAX_COMPONENT_ROWS
                            || got_total_bytes > MAX_COMPONENT_BYTES as u64
                        {
                            return Err(self.fail_closed(StorageHostError::Bounds));
                        }
                        if page.is_empty() && !done {
                            return Err(self.fail_closed(StorageHostError::Protocol));
                        }
                        if revision.is_some_and(|value| value != got_revision)
                            || total_rows.is_some_and(|value| value != got_total_rows)
                            || total_bytes.is_some_and(|value| value != got_total_bytes)
                        {
                            return Err(self.fail_closed(StorageHostError::Protocol));
                        }
                        revision = Some(got_revision);
                        total_rows = Some(got_total_rows);
                        total_bytes = Some(got_total_bytes);
                        let page_bytes: usize = page.iter().map(|row| row.value.len()).sum();
                        if rows.len().saturating_add(page.len()) > MAX_COMPONENT_ROWS
                            || rows
                                .iter()
                                .map(|row: &StorageRow| row.value.len())
                                .sum::<usize>()
                                .saturating_add(page_bytes)
                                > MAX_COMPONENT_BYTES
                        {
                            return Err(self.fail_closed(StorageHostError::Bounds));
                        }
                        let mut mapped = Vec::with_capacity(page.len());
                        for row in page {
                            validate_key(&row.key)?;
                            if row.value.len() > MAX_COMPONENT_VALUE_BYTES {
                                return Err(self.fail_closed(StorageHostError::Bounds));
                            }
                            if !seen.insert(row.key.clone()) {
                                return Err(self.fail_closed(StorageHostError::Protocol));
                            }
                            mapped.push(StorageRow {
                                key: row.key,
                                value: row.value,
                            });
                        }
                        start = match start
                            .checked_add(u32::try_from(mapped.len()).unwrap_or(u32::MAX))
                        {
                            Some(next) => next,
                            None => return Err(self.fail_closed(StorageHostError::Protocol)),
                        };
                        rows.extend(mapped);
                        if done {
                            let bytes = rows.iter().map(|row| row.value.len() as u64).sum::<u64>();
                            if rows.len() != got_total_rows as usize || bytes != got_total_bytes {
                                return Err(self.fail_closed(StorageHostError::Protocol));
                            }
                            return Ok((got_revision, rows));
                        }
                    }
                    Message::Error { kind, .. } => return Err(self.component_error(kind)),
                    _ => return Err(self.fail_closed(StorageHostError::Protocol)),
                }
            }
        }

        /// Atomically commit one bounded set of rows. A mutation is never
        /// replayed; any error after submission poisons the connection.
        ///
        /// # Errors
        ///
        /// Returns [`StorageHostError::Bounds`] for invalid keys/values or an
        /// over-bound batch and the fail-closed protocol/component errors
        /// otherwise.
        pub fn commit_batch(
            &self,
            namespace: &str,
            expected_revision: u64,
            upserts: Vec<StorageRow>,
            deletes: Vec<String>,
            deadline: Duration,
        ) -> Result<StorageCommit, StorageHostError> {
            validate_namespace(namespace)?;
            if upserts.len() > MAX_BATCH_ROWS || deletes.len() > MAX_BATCH_ROWS {
                return Err(StorageHostError::Bounds);
            }
            let mut batch_bytes = 0usize;
            let mut seen = std::collections::BTreeSet::new();
            let mut wire_upserts = Vec::with_capacity(upserts.len());
            for row in upserts {
                validate_key(&row.key)?;
                if row.value.len() > MAX_COMPONENT_VALUE_BYTES {
                    return Err(StorageHostError::Bounds);
                }
                if !seen.insert(row.key.clone()) {
                    return Err(StorageHostError::Bounds);
                }
                batch_bytes = batch_bytes
                    .saturating_add(row.key.len())
                    .saturating_add(row.value.len())
                    .saturating_add(8);
                wire_upserts.push(Row {
                    key: row.key,
                    value: row.value,
                });
            }
            for key in &deletes {
                validate_key(key)?;
                if !seen.insert(key.clone()) {
                    return Err(StorageHostError::Bounds);
                }
                batch_bytes = batch_bytes.saturating_add(key.len()).saturating_add(4);
            }
            if batch_bytes > MAX_BATCH_BYTES {
                return Err(StorageHostError::Bounds);
            }
            let upsert_count = wire_upserts.len();
            let delete_count = deletes.len();
            let request = self.next_request()?;
            let response = self.exchange(
                Message::CommitBatch {
                    request,
                    namespace: namespace.to_owned(),
                    expected_revision,
                    upserts: wire_upserts,
                    deletes,
                },
                deadline,
                true,
            )?;
            match response {
                Message::Committed {
                    request: got,
                    namespace: got_name,
                    revision,
                    rows_written,
                    rows_removed,
                    total_rows,
                    total_bytes,
                    ..
                } if got == request && got_name == namespace => {
                    if rows_written as usize > upsert_count
                        || rows_removed as usize > delete_count
                        || total_rows as usize > MAX_COMPONENT_ROWS
                        || total_bytes > MAX_COMPONENT_BYTES as u64
                    {
                        return Err(self.fail_closed(StorageHostError::Bounds));
                    }
                    Ok(StorageCommit {
                        revision,
                        submitted: true,
                    })
                }
                Message::Error {
                    request: got, kind, ..
                } if got == request => Err(self.component_error(kind)),
                _ => Err(self.fail_closed(StorageHostError::Protocol)),
            }
        }

        /// Restore the previous revision as a new revision. Like
        /// [`Self::commit_batch`], a submitted rollback is never replayed.
        ///
        /// # Errors
        ///
        /// Returns the fail-closed protocol/component errors.
        pub fn rollback(
            &self,
            namespace: &str,
            expected_revision: u64,
            deadline: Duration,
        ) -> Result<u64, StorageHostError> {
            validate_namespace(namespace)?;
            let request = self.next_request()?;
            let response = self.exchange(
                Message::Rollback {
                    request,
                    namespace: namespace.to_owned(),
                    expected_revision,
                },
                deadline,
                true,
            )?;
            match response {
                Message::RolledBack {
                    request: got,
                    namespace: got_name,
                    revision,
                    total_rows,
                    total_bytes,
                    ..
                } if got == request && got_name == namespace => {
                    if total_rows as usize > MAX_COMPONENT_ROWS
                        || total_bytes > MAX_COMPONENT_BYTES as u64
                    {
                        return Err(self.fail_closed(StorageHostError::Bounds));
                    }
                    Ok(revision)
                }
                Message::Error { kind, .. } => Err(self.component_error(kind)),
                _ => Err(self.fail_closed(StorageHostError::Protocol)),
            }
        }

        /// Ask the component to release locks and exit, then reap the recorded
        /// process group within bounded grace.
        ///
        /// # Errors
        ///
        /// Returns the fail-closed protocol/component errors.
        pub fn shutdown(&self, deadline: Duration) -> Result<(), StorageHostError> {
            let mut inner = self.lock()?;
            if inner.shutdown_requested {
                return Ok(());
            }
            if inner.poisoned {
                return Err(StorageHostError::Poisoned);
            }
            let write = match inner.stdin.as_mut() {
                Some(stdin) => protocol::write_frame(stdin, &Message::Shutdown),
                None => return Err(StorageHostError::Poisoned),
            };
            if write.is_err() {
                self.kill_locked(&mut inner);
                return Err(StorageHostError::Spawn);
            }
            match inner.reader.rx.recv_timeout(deadline) {
                Ok(Ok(Message::ShutdownAck)) => {}
                Ok(Ok(_)) => {
                    self.kill_locked(&mut inner);
                    return Err(StorageHostError::Protocol);
                }
                Ok(Err(error)) => {
                    let mapped = wired_error(&self.accounting, &error);
                    self.kill_locked(&mut inner);
                    return Err(mapped);
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.accounting.eof.fetch_add(1, Ordering::Relaxed);
                    self.kill_locked(&mut inner);
                    return Err(StorageHostError::Eof);
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.accounting.timeouts.fetch_add(1, Ordering::Relaxed);
                    self.kill_locked(&mut inner);
                    return Err(StorageHostError::Timeout);
                }
            }
            inner.shutdown_requested = true;
            inner.stdin.take();
            let pid = inner.pid;
            if !wait_child_bounded(&mut inner.child, SHUTDOWN_EXIT_GRACE) {
                stop_child(&mut inner.child, pid);
            }
            inner.reader.join_bounded();
            join_handle_bounded(&mut inner.stderr_thread);
            inner.poisoned = true;
            Ok(())
        }

        fn next_request(&self) -> Result<u64, StorageHostError> {
            let mut inner = self.lock()?;
            if inner.poisoned || inner.shutdown_requested || inner.next_request == 0 {
                return Err(StorageHostError::Poisoned);
            }
            let request = inner.next_request;
            inner.next_request = inner
                .next_request
                .checked_add(1)
                .ok_or(StorageHostError::Poisoned)?;
            Ok(request)
        }

        fn exchange(
            &self,
            message: Message,
            deadline: Duration,
            mutation: bool,
        ) -> Result<Message, StorageHostError> {
            let mut inner = self.lock()?;
            if inner.poisoned || inner.shutdown_requested {
                return Err(StorageHostError::Poisoned);
            }
            if mutation {
                self.submitted.store(true, Ordering::Release);
            }
            // One exchange is in flight at a time. A queued frame or reader
            // error at entry is unsolicited (or terminal) and must fail closed
            // before another request is written.
            match inner.reader.rx.try_recv() {
                Ok(Ok(_)) => {
                    return Err(self.fail_closed_locked(&mut inner, StorageHostError::Protocol));
                }
                Ok(Err(error)) => {
                    let mapped = wired_error(&self.accounting, &error);
                    self.kill_locked(&mut inner);
                    return Err(mapped);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
            }
            let write = match inner.stdin.as_mut() {
                Some(stdin) => protocol::write_frame(stdin, &message),
                None => {
                    self.kill_locked(&mut inner);
                    return Err(StorageHostError::Spawn);
                }
            };
            if write.is_err() {
                let exited = matches!(inner.child.try_wait(), Ok(Some(_)));
                if exited {
                    self.accounting.crashes.fetch_add(1, Ordering::Relaxed);
                }
                self.kill_locked(&mut inner);
                return Err(if exited {
                    StorageHostError::Eof
                } else {
                    StorageHostError::Spawn
                });
            }
            match inner.reader.rx.recv_timeout(deadline) {
                Ok(Ok(response)) => {
                    if matches!(&response, Message::Error { .. }) && mutation {
                        // A mutation error is terminal: the component may have
                        // applied the batch before reporting the failure.
                        self.kill_locked(&mut inner);
                    } else if let Message::Error {
                        request: protocol::CONNECTION_ID,
                        ..
                    } = &response
                    {
                        // Connection-level errors end the exchange lifetime.
                        self.kill_locked(&mut inner);
                    }
                    Ok(response)
                }
                Ok(Err(error)) => {
                    let mapped = wired_error(&self.accounting, &error);
                    self.kill_locked(&mut inner);
                    Err(mapped)
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.accounting.timeouts.fetch_add(1, Ordering::Relaxed);
                    self.kill_locked(&mut inner);
                    Err(StorageHostError::Timeout)
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.accounting.crashes.fetch_add(1, Ordering::Relaxed);
                    self.kill_locked(&mut inner);
                    Err(StorageHostError::Eof)
                }
            }
        }

        fn component_error(&self, kind: ErrorKind) -> StorageHostError {
            self.accounting
                .component_errors
                .fetch_add(1, Ordering::Relaxed);
            StorageHostError::Component(StorageComponentFault::from_wire_code(kind.code()))
        }

        fn fail_closed(&self, error: StorageHostError) -> StorageHostError {
            if let Ok(mut inner) = self.inner.lock() {
                self.fail_closed_locked(&mut inner, error)
            } else {
                let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
                self.fail_closed_locked(&mut inner, error)
            }
        }

        fn fail_closed_locked(
            &self,
            inner: &mut MutexGuard<'_, Inner>,
            error: StorageHostError,
        ) -> StorageHostError {
            match &error {
                StorageHostError::Protocol => {
                    self.accounting
                        .protocol_faults
                        .fetch_add(1, Ordering::Relaxed);
                }
                StorageHostError::Bounds => {
                    self.accounting
                        .bounds_faults
                        .fetch_add(1, Ordering::Relaxed);
                }
                _ => {}
            }
            self.kill_locked(inner);
            error
        }

        fn kill_locked(&self, inner: &mut MutexGuard<'_, Inner>) {
            inner.poisoned = true;
            inner.stdin.take();
            let pid = inner.pid;
            stop_child(&mut inner.child, pid);
            inner.reader.join_bounded();
            join_handle_bounded(&mut inner.stderr_thread);
        }

        fn abort(&self) {
            match self.inner.lock() {
                Ok(mut inner) => self.kill_locked(&mut inner),
                Err(poisoned) => {
                    let mut inner = poisoned.into_inner();
                    self.kill_locked(&mut inner);
                }
            }
        }

        fn lock(&self) -> Result<MutexGuard<'_, Inner>, StorageHostError> {
            self.inner.lock().map_err(|_| StorageHostError::Poisoned)
        }
    }

    impl Drop for StorageComponentClient {
        fn drop(&mut self) {
            let mut inner = match self.inner.lock() {
                Ok(inner) => inner,
                Err(poisoned) => poisoned.into_inner(),
            };
            if inner.shutdown_requested {
                return;
            }
            if !inner.poisoned {
                if let Some(stdin) = inner.stdin.as_mut() {
                    let _ = protocol::write_frame(stdin, &Message::Shutdown);
                }
            }
            inner.stdin.take();
            let pid = inner.pid;
            if !wait_child_bounded(&mut inner.child, DROP_EXIT_GRACE) {
                stop_child(&mut inner.child, pid);
            }
            inner.reader.join_bounded();
            join_handle_bounded(&mut inner.stderr_thread);
        }
    }

    // ── persistence model mapping ───────────────────────────────────────────

    /// Whole-object byte adapter over one component namespace.
    ///
    /// Objects are stored as ordered `chunk-NNNN` rows so a 1 MiB object never
    /// needs a frame larger than the protocol page bound. The adapter accepts
    /// no wire path; the namespace is trusted host state.
    #[derive(Debug)]
    pub struct StorageComponentBytesBackend {
        client: Arc<StorageComponentClient>,
        namespace: String,
        revision: Mutex<u64>,
        deadline: Duration,
    }

    impl StorageComponentBytesBackend {
        /// Open (and lock) one validated namespace.
        ///
        /// # Errors
        ///
        /// Returns [`StorageHostError::Bounds`] for an invalid namespace and
        /// the fail-closed protocol/component errors otherwise.
        pub fn new(
            client: Arc<StorageComponentClient>,
            namespace: String,
            deadline: Duration,
        ) -> Result<Self, StorageHostError> {
            validate_namespace(&namespace)?;
            let revision = client.open_namespace(&namespace, deadline)?;
            Ok(Self {
                client,
                namespace,
                revision: Mutex::new(revision),
                deadline,
            })
        }

        /// Connected client (diagnostics and shared shutdown).
        #[must_use]
        pub fn client(&self) -> &Arc<StorageComponentClient> {
            &self.client
        }

        /// Trusted protocol namespace.
        #[must_use]
        pub fn namespace(&self) -> &str {
            &self.namespace
        }

        /// Read the complete opaque object from bounded component pages.
        ///
        /// # Errors
        ///
        /// Returns [`StorageHostError::Protocol`] when the namespace does not
        /// contain exactly the contiguous chunk rows this adapter wrote.
        pub fn load(&self) -> Result<Option<Vec<u8>>, StorageHostError> {
            let (revision, rows) = self.client.read_snapshot(&self.namespace, self.deadline)?;
            *self
                .revision
                .lock()
                .map_err(|_| StorageHostError::Poisoned)? = revision;
            if rows.is_empty() {
                return Ok(None);
            }
            let mut out = Vec::new();
            for (index, row) in rows.iter().enumerate() {
                let expected = format!("chunk-{index:04}");
                if row.key != expected {
                    return Err(self.client.fail_closed(StorageHostError::Protocol));
                }
                out.extend_from_slice(&row.value);
                if out.len() > MAX_COMPONENT_BYTES {
                    return Err(self.client.fail_closed(StorageHostError::Bounds));
                }
            }
            Ok(Some(out))
        }

        /// Submit a whole-object mutation as bounded chunks. Once submitted,
        /// this method never retries or falls back.
        ///
        /// The v1 batch ceiling ([`MAX_BATCH_BYTES`]) is lower than the
        /// snapshot ceiling, so an object whose encoded upsert/delete batch
        /// would exceed it is refused with [`StorageHostError::Bounds`] before
        /// any mutation. Whole-object atomic commits above that ceiling need a
        /// transaction extension in the component protocol.
        ///
        /// # Errors
        ///
        /// Returns [`StorageHostError::Bounds`] for an over-bound object or
        /// batch and the fail-closed protocol/component errors otherwise.
        pub fn commit(&self, bytes: &[u8]) -> Result<StorageCommit, StorageHostError> {
            if bytes.len() > MAX_COMPONENT_BYTES {
                return Err(StorageHostError::Bounds);
            }
            let (revision, old_rows) = self.client.read_snapshot(&self.namespace, self.deadline)?;
            *self
                .revision
                .lock()
                .map_err(|_| StorageHostError::Poisoned)? = revision;
            let mut upserts = Vec::new();
            for (index, chunk) in bytes.chunks(MAX_COMPONENT_VALUE_BYTES).enumerate() {
                upserts.push(StorageRow {
                    key: format!("chunk-{index:04}"),
                    value: chunk.to_vec(),
                });
            }
            if bytes.is_empty() {
                upserts.push(StorageRow {
                    key: "chunk-0000".to_owned(),
                    value: Vec::new(),
                });
            }
            for row in &old_rows {
                if !row.key.starts_with("chunk-") {
                    return Err(self.client.fail_closed(StorageHostError::Protocol));
                }
            }
            let deletes = old_rows
                .into_iter()
                .filter(|row| !upserts.iter().any(|new| new.key == row.key))
                .map(|row| row.key)
                .collect();
            let committed = self.client.commit_batch(
                &self.namespace,
                revision,
                upserts,
                deletes,
                self.deadline,
            )?;
            *self
                .revision
                .lock()
                .map_err(|_| StorageHostError::Poisoned)? = committed.revision;
            Ok(committed)
        }

        /// Roll back one revision without replaying the request.
        ///
        /// # Errors
        ///
        /// Returns the fail-closed protocol/component errors.
        pub fn rollback(&self) -> Result<u64, StorageHostError> {
            let expected = *self
                .revision
                .lock()
                .map_err(|_| StorageHostError::Poisoned)?;
            let revision = self
                .client
                .rollback(&self.namespace, expected, self.deadline)?;
            *self
                .revision
                .lock()
                .map_err(|_| StorageHostError::Poisoned)? = revision;
            Ok(revision)
        }
    }

    /// Component-backed [`SessionFileBackend`].
    ///
    /// The linked codec remains the byte authority: encode/decode delegate to
    /// [`StorageSessionBackend`], so component and linked adapters persist
    /// identical bytes. Paths passed by the runtime are used only as local
    /// namespace keys (hashed); no path travels on the wire.
    ///
    /// `session_file`/`session_file_for` return `None` on purpose: startup
    /// discovery and migration are deferred until storage PR #18 merges and is
    /// pinned, and the linked backend remains the default.
    #[derive(Debug)]
    pub struct StorageComponentSessionBackend {
        client: Arc<StorageComponentClient>,
        root: PathBuf,
        deadline: Duration,
        backends: Mutex<std::collections::BTreeMap<String, Arc<StorageComponentBytesBackend>>>,
    }

    impl StorageComponentSessionBackend {
        /// Build a component session adapter over an existing canonical
        /// session directory.
        ///
        /// # Errors
        ///
        /// Returns [`StorageHostError::Confinement`] when the directory cannot
        /// be canonicalized or is not a directory.
        pub fn new(
            client: Arc<StorageComponentClient>,
            session_root: impl Into<PathBuf>,
            deadline: Duration,
        ) -> Result<Self, StorageHostError> {
            let root = confine_directory(session_root.into())?;
            Ok(Self {
                client,
                root,
                deadline,
                backends: Mutex::new(std::collections::BTreeMap::new()),
            })
        }

        fn backend_for(
            &self,
            path: &Path,
        ) -> Result<Arc<StorageComponentBytesBackend>, StorageHostError> {
            let namespace = namespace_for_path('s', &self.root, path)?;
            let mut backends = self
                .backends
                .lock()
                .map_err(|_| StorageHostError::Poisoned)?;
            if let Some(backend) = backends.get(&namespace) {
                return Ok(Arc::clone(backend));
            }
            let backend = Arc::new(StorageComponentBytesBackend::new(
                Arc::clone(&self.client),
                namespace.clone(),
                self.deadline,
            )?);
            backends.insert(namespace, Arc::clone(&backend));
            Ok(backend)
        }
    }

    impl SessionFileBackend for StorageComponentSessionBackend {
        fn encode_snapshot(&self, snap: &SessionSnapshot) -> Result<Vec<u8>, SessionError> {
            StorageSessionBackend::new().encode_snapshot(snap)
        }

        fn decode_snapshot(&self, bytes: &[u8]) -> Result<SessionSnapshot, SessionError> {
            StorageSessionBackend::new().decode_snapshot(bytes)
        }

        fn commit_session_bytes(&self, path: &Path, bytes: &[u8]) -> Result<(), SessionError> {
            if bytes.len() > MAX_SESSION_FILE_BYTES {
                return Err(SessionError::TooLarge {
                    what: "session component payload",
                    actual: bytes.len(),
                    limit: MAX_SESSION_FILE_BYTES,
                });
            }
            let backend = self.backend_for(path).map_err(session_error)?;
            backend.commit(bytes).map_err(session_error)?;
            Ok(())
        }

        fn load_session_bytes(&self, path: &Path) -> Result<Vec<u8>, SessionError> {
            let backend = self.backend_for(path).map_err(session_error)?;
            match backend.load().map_err(session_error)? {
                Some(bytes) => Ok(bytes),
                None => Err(SessionError::NotFound),
            }
        }

        fn session_file_for(
            &self,
            _xdg_state_home: Option<&str>,
            _home: Option<&str>,
        ) -> Option<PathBuf> {
            None
        }

        fn session_file(&self) -> Option<PathBuf> {
            None
        }
    }

    /// Component-backed [`KvCommitBackend`] for one plugin store directory.
    ///
    /// The caller's Core-owned quotas run before this adapter. Paths are local
    /// namespace keys only; the wire sees a hashed namespace and opaque bytes.
    #[derive(Debug)]
    pub struct StorageComponentKvBackend {
        client: Arc<StorageComponentClient>,
        root: PathBuf,
        deadline: Duration,
        backends: Mutex<std::collections::BTreeMap<String, Arc<StorageComponentBytesBackend>>>,
    }

    impl StorageComponentKvBackend {
        /// Build a component KV adapter over an existing canonical store
        /// directory.
        ///
        /// # Errors
        ///
        /// Returns [`StorageHostError::Confinement`] when the directory cannot
        /// be canonicalized or is not a directory.
        pub fn new(
            client: Arc<StorageComponentClient>,
            store_root: impl Into<PathBuf>,
            deadline: Duration,
        ) -> Result<Self, StorageHostError> {
            let root = confine_directory(store_root.into())?;
            Ok(Self {
                client,
                root,
                deadline,
                backends: Mutex::new(std::collections::BTreeMap::new()),
            })
        }

        fn backend_for(
            &self,
            path: &Path,
        ) -> Result<Arc<StorageComponentBytesBackend>, StorageHostError> {
            let namespace = namespace_for_path('k', &self.root, path)?;
            let mut backends = self
                .backends
                .lock()
                .map_err(|_| StorageHostError::Poisoned)?;
            if let Some(backend) = backends.get(&namespace) {
                return Ok(Arc::clone(backend));
            }
            let backend = Arc::new(StorageComponentBytesBackend::new(
                Arc::clone(&self.client),
                namespace.clone(),
                self.deadline,
            )?);
            backends.insert(namespace, Arc::clone(&backend));
            Ok(backend)
        }
    }

    impl KvCommitBackend for StorageComponentKvBackend {
        fn commit_store_bytes(&self, path: &Path, bytes: &[u8]) -> Result<(), KvCommitError> {
            if bytes.len() > STORE_FILE_MAX_BYTES {
                return Err(KvCommitError::new("store image exceeds the Core ceiling"));
            }
            let backend = self
                .backend_for(path)
                .map_err(|error| KvCommitError::new(error.to_string()))?;
            backend
                .commit(bytes)
                .map_err(|error| KvCommitError::new(error.to_string()))?;
            Ok(())
        }

        fn load_store_bytes(&self, path: &Path) -> Result<Option<Vec<u8>>, KvCommitError> {
            let backend = self
                .backend_for(path)
                .map_err(|error| KvCommitError::new(error.to_string()))?;
            backend
                .load()
                .map_err(|error| KvCommitError::new(error.to_string()))
        }
    }

    fn session_error(error: StorageHostError) -> SessionError {
        SessionError::Io(error.to_string())
    }

    // ── explicit migration ──────────────────────────────────────────────────

    /// Explicitly copy one existing linked file into a component namespace.
    ///
    /// The caller names both the linked source root and file, plus the
    /// prepared component root and marker file. The source is only read; the
    /// linked path is never removed or renamed. A marker makes the operation
    /// idempotent, and every path must stay under its explicit canonical root.
    ///
    /// # Errors
    ///
    /// Returns [`StorageHostError::Confinement`] for escaping/symlinked paths,
    /// [`StorageHostError::Bounds`] for an over-bound source, and the
    /// fail-closed component errors otherwise.
    pub fn migrate_linked_file(
        backend: &StorageComponentBytesBackend,
        linked_root: &Path,
        linked_file: &Path,
        marker_root: &Path,
        marker_file: &Path,
    ) -> Result<StorageMigrationOutcome, StorageHostError> {
        let linked_root = confine_directory(linked_root.to_path_buf())?;
        let marker_root = confine_directory(marker_root.to_path_buf())?;
        let linked_relative = confined_relative(&linked_root, linked_file)?;
        let linked = linked_root.join(&linked_relative);
        match std::fs::symlink_metadata(&linked) {
            Ok(metadata)
                if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(StorageHostError::Confinement),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(StorageMigrationOutcome::NoSource);
            }
            Err(_) => return Err(StorageHostError::Confinement),
        }
        let marker = confine_new_file(&marker_root, marker_file)?;
        match std::fs::symlink_metadata(&marker) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                    return Err(StorageHostError::Confinement);
                }
                return Ok(StorageMigrationOutcome::AlreadyMigrated);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(StorageHostError::Confinement),
        }
        let bytes = match bitty_storage::atomic_io::load_bytes_capped(&linked, MAX_COMPONENT_BYTES)
        {
            Ok(bytes) => bytes,
            Err(bitty_storage::atomic_io::LoadError::NotFound) => {
                return Ok(StorageMigrationOutcome::NoSource);
            }
            Err(bitty_storage::atomic_io::LoadError::TooLarge { .. }) => {
                return Err(StorageHostError::Bounds);
            }
            Err(bitty_storage::atomic_io::LoadError::Io(_)) => return Err(StorageHostError::Io),
        };
        backend.commit(&bytes)?;
        bitty_storage::atomic_io::save_bytes_atomic(
            &marker,
            MIGRATION_MARKER,
            MIGRATION_MARKER_MAX_BYTES,
        )
        .map_err(|_| StorageHostError::Io)?;
        Ok(StorageMigrationOutcome::Migrated { bytes: bytes.len() })
    }

    fn confine_directory(path: PathBuf) -> Result<PathBuf, StorageHostError> {
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            if metadata.file_type().is_symlink() {
                return Err(StorageHostError::Confinement);
            }
        }
        let root = std::fs::canonicalize(&path).map_err(|_| StorageHostError::Confinement)?;
        let metadata =
            std::fs::symlink_metadata(&root).map_err(|_| StorageHostError::Confinement)?;
        if !metadata.file_type().is_dir() {
            return Err(StorageHostError::Confinement);
        }
        Ok(root)
    }

    fn confine_new_file(root: &Path, path: &Path) -> Result<PathBuf, StorageHostError> {
        Ok(root.join(confined_relative(root, path)?))
    }

    fn confined_relative(root: &Path, path: &Path) -> Result<PathBuf, StorageHostError> {
        if !path.is_absolute() {
            return Err(StorageHostError::Confinement);
        }
        let parent = path.parent().ok_or(StorageHostError::Confinement)?;
        let parent = std::fs::canonicalize(parent).map_err(|_| StorageHostError::Confinement)?;
        if !parent.starts_with(root) {
            return Err(StorageHostError::Confinement);
        }
        let name = path.file_name().ok_or(StorageHostError::Confinement)?;
        let full = parent.join(name);
        let relative = full
            .strip_prefix(root)
            .map_err(|_| StorageHostError::Confinement)?
            .to_path_buf();
        if relative.as_os_str().is_empty() {
            return Err(StorageHostError::Confinement);
        }
        Ok(relative)
    }

    fn wired_error(accounting: &StorageHostAccounting, error: &WireError) -> StorageHostError {
        match error {
            WireError::Io(_) | WireError::Truncated => {
                accounting.eof.fetch_add(1, Ordering::Relaxed);
                StorageHostError::Eof
            }
            _ => {
                accounting.protocol_faults.fetch_add(1, Ordering::Relaxed);
                StorageHostError::Protocol
            }
        }
    }

    fn namespace_for_path(
        prefix: char,
        root: &Path,
        path: &Path,
    ) -> Result<String, StorageHostError> {
        let relative = confined_relative(root, path)?;
        let text = relative.to_str().ok_or(StorageHostError::Confinement)?;
        let digest = bitty_package::integrity::sha256_hex(text.as_bytes());
        Ok(format!("{prefix}{}", &digest[..32]))
    }

    // ── process helpers ─────────────────────────────────────────────────────

    fn spawn_stderr_reader(
        stderr: impl Read + Send + 'static,
        ring: Arc<Mutex<StderrRing>>,
    ) -> io::Result<JoinHandle<()>> {
        thread::Builder::new()
            .name("bitty-storage-stderr".to_owned())
            .spawn(move || {
                let mut stderr = stderr;
                let mut buffer = [0u8; 4096];
                loop {
                    match stderr.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            let mut ring = ring.lock().unwrap_or_else(PoisonError::into_inner);
                            ring.push(&buffer[..count]);
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            })
    }

    fn join_handle_bounded(handle: &mut Option<JoinHandle<()>>) {
        let deadline = Instant::now()
            .checked_add(THREAD_JOIN_TIMEOUT)
            .unwrap_or_else(Instant::now);
        while Instant::now() < deadline {
            if handle.as_ref().is_none_or(JoinHandle::is_finished) {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        if handle.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(thread) = handle.take() {
                let _ = thread.join();
            }
        } else {
            // A descendant outside the recorded process group may still hold
            // the pipe. Detach instead of blocking Core shutdown.
            handle.take();
        }
    }

    fn wait_child_bounded(child: &mut Child, grace: Duration) -> bool {
        let deadline = Instant::now()
            .checked_add(grace)
            .unwrap_or_else(Instant::now);
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => thread::sleep(Duration::from_millis(2)),
                Err(_) => return false,
            }
        }
        matches!(child.try_wait(), Ok(Some(_)))
    }

    fn stop_child(child: &mut Child, pid: u32) {
        kill_process_group(pid);
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    fn isolate_process_group(command: &mut Command) {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }

    #[cfg(windows)]
    fn isolate_process_group(command: &mut Command) {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0000_0200);
    }

    #[cfg(not(any(unix, windows)))]
    fn isolate_process_group(_command: &mut Command) {}

    #[cfg(unix)]
    fn kill_process_group(pid: u32) {
        if let Ok(pid) = i32::try_from(pid) {
            if pid > 0 {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
    }

    #[cfg(not(unix))]
    fn kill_process_group(_pid: u32) {}

    // ── grammar and bounds ──────────────────────────────────────────────────

    fn validate_namespace(value: &str) -> Result<(), StorageHostError> {
        if valid_name(value, MAX_NAMESPACE_BYTES) {
            Ok(())
        } else {
            Err(StorageHostError::Bounds)
        }
    }

    fn validate_key(value: &str) -> Result<(), StorageHostError> {
        if valid_name(value, MAX_KEY_BYTES) {
            Ok(())
        } else {
            Err(StorageHostError::Bounds)
        }
    }
}

#[cfg(feature = "storage-component")]
#[allow(unused_imports)]
pub use enabled::{
    StorageComponentBytesBackend, StorageComponentClient, StorageComponentKvBackend,
    StorageComponentSessionBackend, migrate_linked_file,
};

#[cfg(not(feature = "storage-component"))]
/// Feature-disabled marker: default builds cannot accidentally spawn storage.
#[derive(Debug)]
pub struct StorageComponentClient {
    accounting: Arc<StorageHostAccounting>,
}

#[cfg(not(feature = "storage-component"))]
impl StorageComponentClient {
    /// The protocol client is unavailable until the explicit feature is built.
    ///
    /// # Errors
    ///
    /// Always [`StorageHostError::Denied`] while the feature is off.
    pub fn connect(_config: &StorageComponentConfig) -> Result<Arc<Self>, StorageHostError> {
        Err(StorageHostError::Denied)
    }

    /// Shared accounting counters (all zero while disabled).
    #[must_use]
    pub fn accounting(&self) -> &Arc<StorageHostAccounting> {
        &self.accounting
    }

    /// No mutation can be submitted while the feature is off.
    #[must_use]
    pub const fn commit_was_submitted(&self) -> bool {
        false
    }

    /// No child exists while disabled.
    #[must_use]
    pub fn stderr_tail(&self) -> String {
        String::new()
    }

    /// No child is running while disabled.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        false
    }
}

// ── shared grammar and path helpers ─────────────────────────────────────────

/// `[a-z0-9][a-z0-9._-]{0,max-1}` with no `..` dot segment. Mirrors the
/// protocol v1 namespace/key grammar without depending on the optional crate.
fn valid_name(value: &str, max: usize) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > max {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    if !bytes.iter().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
    }) {
        return false;
    }
    !value.contains("..")
}

#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> Result<(), StorageHostError> {
    use std::os::unix::fs::PermissionsExt as _;
    let permissions = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(path, permissions).map_err(|_| StorageHostError::Confinement)
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) -> Result<(), StorageHostError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_gate_is_denied() {
        assert!(!StorageComponentGate::DISABLED.allows());
        assert!(!StorageComponentGate::default().allows());
        assert!(
            !StorageComponentGate {
                preferred: true,
                safe_mode: true,
                capability_granted: true,
            }
            .allows()
        );
        assert!(
            !StorageComponentGate {
                preferred: true,
                safe_mode: false,
                capability_granted: false,
            }
            .allows()
        );
        assert!(
            StorageComponentGate {
                preferred: true,
                safe_mode: false,
                capability_granted: true,
            }
            .allows()
        );
    }

    #[test]
    fn fault_table_is_extensible_and_unknown_is_generic() {
        assert_eq!(
            StorageComponentFault::from_wire_code(1),
            StorageComponentFault::Protocol
        );
        assert_eq!(
            StorageComponentFault::from_wire_code(10),
            StorageComponentFault::Internal
        );
        assert_eq!(
            StorageComponentFault::from_wire_code(11),
            StorageComponentFault::Uncertain
        );
        assert_eq!(
            StorageComponentFault::from_wire_code(0),
            StorageComponentFault::Unknown
        );
        assert_eq!(
            StorageComponentFault::from_wire_code(255),
            StorageComponentFault::Unknown
        );
    }

    #[test]
    fn grammar_rejects_traversal_and_non_ascii() {
        assert!(valid_name("session-1", MAX_NAMESPACE_BYTES));
        assert!(valid_name("chunk-0000", MAX_KEY_BYTES));
        assert!(!valid_name("", MAX_KEY_BYTES));
        assert!(!valid_name("../escape", MAX_KEY_BYTES));
        assert!(!valid_name("a..b", MAX_KEY_BYTES));
        assert!(!valid_name("Upper", MAX_KEY_BYTES));
        assert!(!valid_name("with/slash", MAX_KEY_BYTES));
        assert!(!valid_name("with space", MAX_KEY_BYTES));
    }

    #[test]
    fn root_selection_rejects_invalid_profiles_and_relative_bases() {
        let absolute = std::env::temp_dir();
        assert!(StorageRootSelection::under(&absolute, "default").is_ok());
        assert!(StorageRootSelection::under(&absolute, "../default").is_err());
        assert!(StorageRootSelection::under("relative", "default").is_err());
        assert!(StorageRootSelection::under(&absolute, "Upper").is_err());
    }

    #[test]
    fn component_config_defaults_off_with_standard_deadlines() {
        let root = StorageRootSelection::under(std::env::temp_dir(), "default").expect("root");
        let config = StorageComponentConfig::disabled(root);
        assert_eq!(config.gate, StorageComponentGate::DISABLED);
        assert_eq!(config.operation_deadline, DEFAULT_OPERATION_DEADLINE);
        assert_eq!(config.handshake_timeout, HANDSHAKE_TIMEOUT);
        assert_eq!(config.shutdown_timeout, SHUTDOWN_TIMEOUT);
    }
}
