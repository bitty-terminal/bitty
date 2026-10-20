//! Last-activation projection for `bitty extension list` (CTX-1361, #2254).
//!
//! `bitty extension list` is a local-class command: it never starts an
//! instance, opens IPC, or executes a component, so it cannot read a live
//! broker's in-memory slots. Instead, the broker records each activation
//! transition it already makes (spawn, crash, idle/shutdown stop) and each
//! fail-closed denial into one bounded plain-text record under the user state
//! directory:
//!
//! ```text
//! <state-root>/<name>/activation     # "v1 active\n", "v1 denied:capability_denied\n"
//! ```
//!
//! The state root is `$XDG_STATE_HOME/bitty/extensions`
//! (`$HOME/.local/state/bitty/extensions` when `XDG_STATE_HOME` is unset).
//! It is independent of the install tiers, so the record covers user and
//! system installs alike and survives reinstalls. A user-tier record written
//! beside `current` by older builds is still read as a fallback; new writes
//! only go to the state directory.
//!
//! This is a projection of [`ComponentState`](super::ComponentState) and the
//! stable [`ComponentHostError::class`](super::ComponentHostError) labels, not
//! a second state machine: the broker writes it exactly at the transition
//! points it already handles, and [`read_activation`] is the only reader.
//! The label is a last-recorded transition, never a liveness probe: `active`
//! means a spawn was recorded, not that the process is running now. A
//! missing, oversized, non-regular, or malformed record reads as
//! [`ActivationState::Installed`] ("no activation recorded"), never as an
//! unearned `active`.
//!
//! Both sides treat the record path as hostile filesystem input:
//!
//! - The reader opens the record once without following a link
//!   (`O_NOFOLLOW`/`FILE_FLAG_OPEN_REPARSE_POINT`), takes its length from that
//!   handle, and reads at most
//!   [`COMPONENT_ACTIVATION_MAX_BYTES`]` + 1` bytes through it, so a symlink
//!   swapped in after a separate `symlink_metadata` cannot redirect or unbound
//!   the read. A symlinked component record directory is refused too.
//! - The writer creates a fresh sibling temp record with `create_new` (which
//!   refuses a planted symlink), writes and flushes it, renames it over
//!   `activation`, and flushes the directory. A failed creation never removes
//!   an entry the writer did not create. A symlinked component record
//!   directory is refused; only a missing real directory is created.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Record file name inside one component's state directory.
pub const COMPONENT_ACTIVATION_FILE: &str = "activation";

/// Directory under the Bitty state directory holding activation records.
///
/// Uses the approved `extensions/` name: the directory-semantics contract
/// freezes `components/` for boot-required units and historic identifiers,
/// and forbids new on-disk roots under that name.
pub const COMPONENT_ACTIVATION_DIR_NAME: &str = "extensions";

/// Test-only override for the component state root (not a user
/// configuration surface; mirrors [`super::COMPONENTS_DIR_ENV`]).
pub const COMPONENT_STATE_DIR_ENV: &str = "BITTY_COMPONENT_STATE_DIR";

/// Prefix the writer uses for temporary records beside `activation`.
const ACTIVATION_TEMP_PREFIX: &str = "activation.tmp-";

/// Maximum bytes read from or written to one activation record.
///
/// The longest record is `v1 denied:component_untrusted\n` (31 bytes); the
/// bound leaves room for future denial classes without growing unbounded.
pub const COMPONENT_ACTIVATION_MAX_BYTES: u64 = 64;

/// Record format prefix (`v1`).
const ACTIVATION_FORMAT_PREFIX: &str = "v1";

/// Fresh temporary names the writer tries before giving up.
///
/// A planted `activation.tmp-*` entry only costs one attempt; eight attempts
/// make an accidental collision vanishingly unlikely without an unbounded
/// loop in a broker transition path.
const ACTIVATION_TEMP_ATTEMPTS: u32 = 8;

/// Why the last request for a component failed closed before a process ran.
///
/// The variants are the closed subset of
/// [`ComponentHostError::class`](super::ComponentHostError) labels that mean
/// "denied", so a record can never name a free-form string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationDenial {
    /// The plugin's granted capabilities do not authorize the request.
    CapabilityDenied,
    /// The requesting plugin is not in the `Activated` lifecycle state.
    PluginNotActive,
    /// The plugin's grant could not be computed (declared authority invalid).
    GrantInvalid,
    /// The installed component failed digest/descriptor verification.
    Untrusted,
    /// No installed component resolved for the request.
    Unavailable,
}

impl ActivationDenial {
    /// Every denial class a record may carry.
    pub const ALL: [Self; 5] = [
        Self::CapabilityDenied,
        Self::PluginNotActive,
        Self::GrantInvalid,
        Self::Untrusted,
        Self::Unavailable,
    ];

    /// Stable machine label (identical to the host error class).
    #[must_use]
    pub fn class(self) -> &'static str {
        match self {
            Self::CapabilityDenied => "capability_denied",
            Self::PluginNotActive => "plugin_not_active",
            Self::GrantInvalid => "component_grant_invalid",
            Self::Untrusted => "component_untrusted",
            Self::Unavailable => "component_unavailable",
        }
    }

    fn from_class(class: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|denial| denial.class() == class)
    }
}

/// Last recorded activation of one component name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationState {
    /// Installed, but no activation has been recorded (never requested
    /// since install, or the record is absent/unreadable).
    Installed,
    /// The broker spawned and handshook the component for a request.
    Active,
    /// The last request was refused before any spawn, naming the class.
    Denied(ActivationDenial),
    /// The last recorded process ended as a crash (including the crash
    /// latch that makes the component unavailable until restart).
    Crashed,
    /// The last recorded process stopped cleanly (idle stop or shutdown).
    Stopped,
}

impl ActivationState {
    /// Stable table label (`installed`, `active`, `denied:<class>`,
    /// `crashed`, `stopped`).
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Installed => "installed".to_owned(),
            Self::Active => "active".to_owned(),
            Self::Denied(denial) => format!("denied:{}", denial.class()),
            Self::Crashed => "crashed".to_owned(),
            Self::Stopped => "stopped".to_owned(),
        }
    }

    /// Named error class for a denial, `None` for every other state.
    #[must_use]
    pub fn class(self) -> Option<&'static str> {
        match self {
            Self::Denied(denial) => Some(denial.class()),
            _ => None,
        }
    }

    fn parse(text: &str) -> Option<Self> {
        let rest = text
            .strip_prefix(ACTIVATION_FORMAT_PREFIX)?
            .strip_prefix(' ')?;
        match rest {
            "installed" => Some(Self::Installed),
            "active" => Some(Self::Active),
            "crashed" => Some(Self::Crashed),
            "stopped" => Some(Self::Stopped),
            other => other
                .strip_prefix("denied:")
                .and_then(ActivationDenial::from_class)
                .map(Self::Denied),
        }
    }

    /// One bounded record line including the trailing newline.
    fn record(self) -> String {
        let mut line = String::with_capacity(32);
        line.push_str(ACTIVATION_FORMAT_PREFIX);
        line.push(' ');
        line.push_str(&self.label());
        line.push('\n');
        line
    }
}

/// Record path inside one component state directory.
#[must_use]
pub fn activation_path(component_dir: &Path) -> PathBuf {
    component_dir.join(COMPONENT_ACTIVATION_FILE)
}

/// `$XDG_STATE_HOME` or `$HOME/.local/state` from explicit environment
/// values (empty or whitespace-only values count as unset).
///
/// Component activation diagnostics live under this user-owned tree rather
/// than under either install tier.
#[must_use]
pub fn state_home_for(xdg_state_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_state_home {
        if !xdg.trim().is_empty() {
            return Some(PathBuf::from(xdg));
        }
    }
    home.filter(|home| !home.trim().is_empty())
        .map(|home| PathBuf::from(home).join(".local").join("state"))
}

/// Component activation state root (`<state-home>/bitty/extensions`).
///
/// The explicit override wins when non-empty (tests). Unlike the install
/// roots, this root is not a trust boundary for executable resolution: it
/// only holds last-recorded diagnostic records.
#[must_use]
pub fn component_state_root_for(
    override_dir: Option<&str>,
    xdg_state_home: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    if let Some(dir) = override_dir {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    state_home_for(xdg_state_home, home)
        .map(|base| base.join("bitty").join(COMPONENT_ACTIVATION_DIR_NAME))
}

/// Open the record for reading without following a link at that path.
///
/// Linux/Android use `rustix` and the BSD/macOS family uses `nix`:
/// `O_NOFOLLOW` makes a symlink fail with `ELOOP` (the record then reads as
/// "not installed"), and `O_NONBLOCK` keeps a planted FIFO from blocking the
/// open. Windows passes `FILE_FLAG_OPEN_REPARSE_POINT` through `std`. Any
/// other platform fails closed instead of reading a path it cannot open
/// without following a link.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn open_record_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::NONBLOCK
        | rustix::fs::OFlags::CLOEXEC;
    let fd = rustix::fs::open(path, flags, rustix::fs::Mode::empty())?;
    Ok(std::fs::File::from(fd))
}

/// Open the record for reading without following a link at that path.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
fn open_record_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    let flags = nix::fcntl::OFlag::O_RDONLY
        | nix::fcntl::OFlag::O_NOFOLLOW
        | nix::fcntl::OFlag::O_NONBLOCK
        | nix::fcntl::OFlag::O_CLOEXEC;
    let fd = nix::fcntl::open(path, flags, nix::sys::stat::Mode::empty())?;
    Ok(std::fs::File::from(fd))
}

/// Open the record for reading without following a link at that path.
#[cfg(windows)]
fn open_record_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

/// Open the record for reading without following a link at that path.
#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))
))]
fn open_record_no_follow(_path: &Path) -> std::io::Result<std::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "activation records are unreadable on this platform",
    ))
}

/// Open the record for reading without following a link at that path.
#[cfg(not(any(unix, windows)))]
fn open_record_no_follow(_path: &Path) -> std::io::Result<std::fs::File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "activation records are unreadable on this platform",
    ))
}

/// Read at most one bounded record through an already-opened handle.
///
/// `metadata` must come from the same handle, so the ceiling and the
/// regular-file test describe the object actually read. A record that grew
/// (or shrank) between the metadata snapshot and the read is rejected rather
/// than parsed: the caller records one short line, and a mismatch means
/// someone else is writing the path.
fn read_record_bytes(file: &std::fs::File, metadata: &std::fs::Metadata) -> Option<Vec<u8>> {
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > COMPONENT_ACTIVATION_MAX_BYTES
    {
        return None;
    }
    let mut bytes = Vec::new();
    if file
        .take(COMPONENT_ACTIVATION_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return None;
    }
    if bytes.len() as u64 != metadata.len() || bytes.len() as u64 > COMPONENT_ACTIVATION_MAX_BYTES {
        return None;
    }
    Some(bytes)
}

/// Read the recorded activation of one component directory.
///
/// Best-effort and bounded: a missing, oversized, symlinked, non-file, or
/// malformed record reads as [`ActivationState::Installed`]. Reading never
/// creates anything, never follows a symlink at the record path, and never
/// reads more than [`COMPONENT_ACTIVATION_MAX_BYTES`] bytes: the file is
/// opened once without following a link, and both the size check and the
/// read use that handle.
#[must_use]
pub fn read_activation(component_dir: &Path) -> ActivationState {
    match std::fs::symlink_metadata(component_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return ActivationState::Installed;
        }
        Ok(_) | Err(_) => {}
    }
    let path = activation_path(component_dir);
    let Ok(file) = open_record_no_follow(&path) else {
        return ActivationState::Installed;
    };
    let Ok(metadata) = file.metadata() else {
        return ActivationState::Installed;
    };
    let Some(bytes) = read_record_bytes(&file, &metadata) else {
        return ActivationState::Installed;
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return ActivationState::Installed;
    };
    ActivationState::parse(text.trim_end_matches(['\n', '\r']))
        .unwrap_or(ActivationState::Installed)
}

/// Read the recorded activation for `name` across both tiers.
///
/// The state directory wins (it is where the broker records now); a legacy
/// user-tier record beside `current` is the next fallback, then a legacy
/// system-tier record. An invalid name reads as
/// [`ActivationState::Installed`] instead of touching an escaped path.
#[must_use]
pub fn read_component_activation(
    user_root: Option<&Path>,
    system_root: Option<&Path>,
    name: &str,
) -> ActivationState {
    read_component_activation_with_state(user_root, system_root, None, name)
}

/// [`read_component_activation`] with the explicit state root.
///
/// The state root covers both install tiers and survives reinstalls; when it
/// holds no record, legacy records beside `current` are still honored.
#[must_use]
pub fn read_component_activation_with_state(
    user_root: Option<&Path>,
    system_root: Option<&Path>,
    state_root: Option<&Path>,
    name: &str,
) -> ActivationState {
    if super::descriptor::validate_component_name(name).is_err() {
        return ActivationState::Installed;
    }
    for root in [state_root, user_root, system_root].into_iter().flatten() {
        let state = read_activation(&root.join(name));
        if state != ActivationState::Installed {
            return state;
        }
    }
    ActivationState::Installed
}

/// Why one activation-record write failed.
///
/// The projection is diagnostics, so the broker drops this value instead of
/// failing a request; it is typed so the writer's guarantees stay testable
/// (a planted link is refused, a taken temp name is retried) and a future
/// caller can tell the cases apart.
#[derive(Debug)]
pub(crate) enum ActivationWriteError {
    /// The record path exists and is not a regular file (symlink, directory).
    RecordNotRegular,
    /// The component record directory is a symlink or not a directory.
    ComponentDirNotRegular,
    /// Creating a missing component record directory failed.
    DirCreate(std::io::Error),
    /// The record would exceed [`COMPONENT_ACTIVATION_MAX_BYTES`].
    RecordOversize,
    /// Every candidate temporary name already existed (`AlreadyExists`).
    TempNameExhausted,
    /// Creating the temporary record failed.
    TempCreate(std::io::Error),
    /// Writing or flushing the temporary record failed.
    TempWrite(std::io::Error),
    /// Renaming the temporary record over `activation` failed.
    Rename(std::io::Error),
    /// Flushing the component directory after the rename failed.
    SyncDir(std::io::Error),
}

impl std::fmt::Display for ActivationWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RecordNotRegular => f.write_str("activation record path is not a regular file"),
            Self::ComponentDirNotRegular => {
                f.write_str("activation component directory is not a real directory")
            }
            Self::DirCreate(error) => write!(f, "activation directory create failed: {error}"),
            Self::RecordOversize => f.write_str("activation record exceeds the byte ceiling"),
            Self::TempNameExhausted => f.write_str("no free activation temp name"),
            Self::TempCreate(error) => write!(f, "activation temp create failed: {error}"),
            Self::TempWrite(error) => write!(f, "activation temp write failed: {error}"),
            Self::Rename(error) => write!(f, "activation rename failed: {error}"),
            Self::SyncDir(error) => write!(f, "activation directory sync failed: {error}"),
        }
    }
}

impl std::error::Error for ActivationWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TempCreate(error)
            | Self::TempWrite(error)
            | Self::Rename(error)
            | Self::SyncDir(error)
            | Self::DirCreate(error) => Some(error),
            Self::RecordNotRegular
            | Self::ComponentDirNotRegular
            | Self::RecordOversize
            | Self::TempNameExhausted => None,
        }
    }
}

/// Create one fresh, empty temporary record without following a link.
///
/// `create_new` maps to `O_CREAT | O_EXCL`, which fails with `AlreadyExists`
/// on a pre-existing entry (including a planted symlink) instead of opening
/// it; Unix additionally requests mode `0600` so the record is owner-only.
fn create_temp_record(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Flush a directory entry so a completed rename survives a crash.
#[cfg(unix)]
fn sync_directory(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// Flush a directory entry so a completed rename survives a crash.
#[cfg(not(unix))]
fn sync_directory(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

fn next_temp_serial() -> u64 {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
}

/// Create one missing component record directory, never through a symlink.
///
/// The state record directory may not exist on first activation. An existing
/// symlink or non-directory is refused instead of followed, matching the
/// record-path rule; creating parent directories is delegated to the OS but
/// the created component directory itself is re-checked.
fn ensure_record_dir(dir: &Path) -> Result<(), ActivationWriteError> {
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ActivationWriteError::ComponentDirNotRegular);
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ActivationWriteError::DirCreate(error)),
    }
    std::fs::create_dir_all(dir).map_err(ActivationWriteError::DirCreate)?;
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(ActivationWriteError::ComponentDirNotRegular)
        }
        Ok(_) => Ok(()),
        Err(error) => Err(ActivationWriteError::DirCreate(error)),
    }
}

/// Remove one component's record and any stale writer temporaries.
///
/// A process killed between temp creation and rename leaves an
/// `activation.tmp-*` entry behind. Such entries are never records, but they
/// keep an otherwise empty component directory from being pruned, so a
/// removed component can linger as "(missing version dir)". Callers that
/// remove the last installed version sweep both the record and those
/// temporaries; unrelated files are left untouched.
pub fn remove_activation_artifacts(component_dir: &Path) {
    let _ = std::fs::remove_file(activation_path(component_dir));
    let Ok(entries) = std::fs::read_dir(component_dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(ACTIVATION_TEMP_PREFIX)
        {
            // Remove the entry itself, including a planted symlink (a
            // symlink reports `is_dir == false`), and never follow it to a
            // target. A same-name directory is writer litter too: versions
            // are strict semver and can never carry this prefix.
            match entry.file_type() {
                Ok(file_type) if file_type.is_dir() => {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
                _ => {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

/// Write one activation record (the broker drops the typed error).
///
/// The write goes through a sibling temp file created with `create_new` and a
/// rename, so a concurrent `bitty extension list` never observes a partial
/// record. A symlinked or non-regular record path is refused, and a temp name
/// that already exists (for example a pre-planted link with the same
/// `<pid>-<serial>` shape) is retried under a fresh name rather than opened:
/// a planted link must never redirect or truncate the write. Failures remove
/// the temp file best-effort and are surfaced as
/// [`ActivationWriteError`]; activation state is diagnostics, never a
/// correctness gate.
pub(crate) fn write_activation(
    component_dir: &Path,
    state: ActivationState,
) -> Result<(), ActivationWriteError> {
    write_activation_with(component_dir, state, &next_temp_serial)
}

/// [`write_activation`] with an injectable temp-name serial (tests plant
/// links at the exact names a write would choose).
fn write_activation_with(
    component_dir: &Path,
    state: ActivationState,
    next_serial: &dyn Fn() -> u64,
) -> Result<(), ActivationWriteError> {
    ensure_record_dir(component_dir)?;
    let path = activation_path(component_dir);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(ActivationWriteError::RecordNotRegular);
        }
        Ok(_) | Err(_) => {}
    }
    let record = state.record();
    if record.len() as u64 > COMPONENT_ACTIVATION_MAX_BYTES {
        return Err(ActivationWriteError::RecordOversize);
    }

    let mut created = None;
    for _ in 0..ACTIVATION_TEMP_ATTEMPTS {
        let temp = component_dir.join(format!(
            "{ACTIVATION_TEMP_PREFIX}{}-{}",
            std::process::id(),
            next_serial()
        ));
        match create_temp_record(&temp) {
            Ok(file) => {
                created = Some((temp, file));
                break;
            }
            // Never remove an entry this writer did not create.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(ActivationWriteError::TempCreate(error)),
        }
    }
    let Some((temp, mut file)) = created else {
        return Err(ActivationWriteError::TempNameExhausted);
    };

    let flushed = file
        .write_all(record.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = flushed {
        let _ = std::fs::remove_file(&temp);
        return Err(ActivationWriteError::TempWrite(error));
    }
    if let Err(error) = std::fs::rename(&temp, &path) {
        let _ = std::fs::remove_file(&temp);
        return Err(ActivationWriteError::Rename(error));
    }
    sync_directory(component_dir).map_err(ActivationWriteError::SyncDir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bitty-ctx1361-activation-{tag}-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn every_state_round_trips_through_the_bounded_record() {
        let dir = scratch("roundtrip");
        assert_eq!(read_activation(&dir), ActivationState::Installed);
        for state in [
            ActivationState::Active,
            ActivationState::Crashed,
            ActivationState::Stopped,
            ActivationState::Installed,
            ActivationState::Denied(ActivationDenial::CapabilityDenied),
            ActivationState::Denied(ActivationDenial::PluginNotActive),
            ActivationState::Denied(ActivationDenial::GrantInvalid),
            ActivationState::Denied(ActivationDenial::Untrusted),
            ActivationState::Denied(ActivationDenial::Unavailable),
        ] {
            write_activation(&dir, state).expect("write record");
            assert_eq!(read_activation(&dir), state, "{}", state.label());
            assert!(
                std::fs::metadata(activation_path(&dir))
                    .expect("record")
                    .len()
                    <= COMPONENT_ACTIVATION_MAX_BYTES
            );
        }
        // The atomic write leaves exactly the record: no temp remnant.
        assert_eq!(std::fs::read_dir(&dir).expect("dir").count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn labels_and_classes_are_stable() {
        assert_eq!(ActivationState::Installed.label(), "installed");
        assert_eq!(ActivationState::Active.label(), "active");
        assert_eq!(ActivationState::Crashed.label(), "crashed");
        assert_eq!(ActivationState::Stopped.label(), "stopped");
        let denied = ActivationState::Denied(ActivationDenial::Untrusted);
        assert_eq!(denied.label(), "denied:component_untrusted");
        assert_eq!(denied.class(), Some("component_untrusted"));
        assert_eq!(ActivationState::Active.class(), None);
        // The denial class set matches the host error classes verbatim.
        assert_eq!(
            ActivationDenial::ALL.map(ActivationDenial::class),
            [
                "capability_denied",
                "plugin_not_active",
                "component_grant_invalid",
                "component_untrusted",
                "component_unavailable",
            ]
        );
    }

    #[test]
    fn hostile_records_read_as_installed() {
        let dir = scratch("hostile");
        let path = activation_path(&dir);
        for bad in [
            "".to_owned(),
            "\n".to_owned(),
            "active\n".to_owned(),               // missing version prefix
            "v1 active-extra\n".to_owned(),      // unknown state
            "v1 denied:made_up\n".to_owned(),    // unknown class
            format!("v1 {}\n", "a".repeat(200)), // oversize
            "v1 act\u{00e9}ve\n".to_owned(),     // multibyte tail
        ] {
            std::fs::write(&path, bad.as_bytes()).expect("bad record");
            assert_eq!(read_activation(&dir), ActivationState::Installed);
        }
        // Non-UTF-8 bytes never panic and never claim a state.
        std::fs::write(&path, [0xff, 0xfe, 0x00]).expect("non-utf8");
        assert_eq!(read_activation(&dir), ActivationState::Installed);
        // A directory at the record path is not a record.
        let _ = std::fs::remove_file(&path);
        std::fs::create_dir(&path).expect("dir record");
        assert_eq!(read_activation(&dir), ActivationState::Installed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_record_is_never_followed() {
        let dir = scratch("symlink");
        let outside = dir.join("outside");
        std::fs::write(&outside, "v1 active\n").expect("outside");
        std::os::unix::fs::symlink(&outside, activation_path(&dir)).expect("symlink");
        assert_eq!(read_activation(&dir), ActivationState::Installed);
        // Writing must not clobber the link target, and must say why.
        assert!(matches!(
            write_activation(&dir, ActivationState::Crashed),
            Err(ActivationWriteError::RecordNotRegular)
        ));
        assert_eq!(
            std::fs::read_to_string(&outside).expect("outside"),
            "v1 active\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn planted_temp_link_is_refused_and_the_write_retries_under_a_fresh_name() {
        let dir = scratch("temp-link");
        let victim = dir.join("victim");
        std::fs::write(&victim, "v1 active\n").expect("victim");
        // A same-user process pre-creates the exact temp name shape a write
        // would pick, as a link to a file it must not be able to redirect.
        let planted_serial = 7;
        let planted = dir.join(format!(
            "{COMPONENT_ACTIVATION_FILE}.tmp-{}-{planted_serial}",
            std::process::id()
        ));
        std::os::unix::fs::symlink(&victim, &planted).expect("planted link");
        let next = AtomicU64::new(planted_serial);
        let result = write_activation_with(&dir, ActivationState::Stopped, &|| {
            next.fetch_add(1, Ordering::Relaxed)
        });
        assert!(result.is_ok(), "{result:?}");
        // The write followed neither the planted link nor any other entry.
        assert_eq!(
            std::fs::read_to_string(&victim).expect("victim"),
            "v1 active\n"
        );
        assert_eq!(
            read_activation(&dir),
            ActivationState::Stopped,
            "the retried temp name must land on the record"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn exhausted_temp_names_surface_a_typed_error() {
        let dir = scratch("temp-exhausted");
        for serial in 0..u64::from(ACTIVATION_TEMP_ATTEMPTS) {
            let planted = dir.join(format!(
                "{COMPONENT_ACTIVATION_FILE}.tmp-{}-{serial}",
                std::process::id()
            ));
            std::os::unix::fs::symlink(dir.join("victim"), &planted).expect("planted link");
        }
        let next = AtomicU64::new(0);
        let result = write_activation_with(&dir, ActivationState::Active, &|| {
            next.fetch_add(1, Ordering::Relaxed)
        });
        assert!(
            matches!(result, Err(ActivationWriteError::TempNameExhausted)),
            "{result:?}"
        );
        // A refused write never creates a record or a link target.
        assert_eq!(read_activation(&dir), ActivationState::Installed);
        assert!(!dir.join("victim").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn grown_record_is_rejected_by_the_handle_bound() {
        let dir = scratch("grown");
        let path = dir.join("record");
        std::fs::write(&path, "v1 active\n").expect("seed");
        let file = std::fs::File::open(&path).expect("open");
        let metadata = file.metadata().expect("metadata");
        // A second writer grows the file after the metadata snapshot. The
        // read through the already-open handle must reject the mismatch
        // instead of parsing an unbounded record.
        {
            let mut writer = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("append");
            writer
                .write_all(&[b'x'; COMPONENT_ACTIVATION_MAX_BYTES as usize * 4])
                .expect("grow");
            writer.sync_all().expect("sync");
        }
        assert!(read_record_bytes(&file, &metadata).is_none());
        // The bound still accepts the unchanged short record it validated.
        let unchanged = dir.join("unchanged");
        std::fs::write(&unchanged, "v1 stopped\n").expect("unchanged");
        let file = std::fs::File::open(&unchanged).expect("open");
        let metadata = file.metadata().expect("metadata");
        assert_eq!(
            read_record_bytes(&file, &metadata).as_deref(),
            Some(b"v1 stopped\n".as_slice())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_oversize_record_is_rejected_without_reading_it() {
        let dir = scratch("symlink-oversize");
        let outside = dir.join("outside");
        std::fs::write(
            &outside,
            format!(
                "v1 active\n{}\n",
                "a".repeat(COMPONENT_ACTIVATION_MAX_BYTES as usize * 8)
            ),
        )
        .expect("outside");
        std::os::unix::fs::symlink(&outside, activation_path(&dir)).expect("symlink");
        assert_eq!(read_activation(&dir), ActivationState::Installed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_tier_record_wins_and_invalid_names_are_inert() {
        let base = scratch("tiers");
        let user = base.join("user");
        let system = base.join("system");
        std::fs::create_dir_all(user.join("net")).expect("user dir");
        std::fs::create_dir_all(system.join("net")).expect("system dir");
        assert_eq!(
            read_component_activation(Some(&user), Some(&system), "net"),
            ActivationState::Installed
        );
        write_activation(&user.join("net"), ActivationState::Active).expect("user record");
        write_activation(&system.join("net"), ActivationState::Stopped).expect("system record");
        assert_eq!(
            read_component_activation(Some(&user), Some(&system), "net"),
            ActivationState::Active
        );
        // System-only installs still report their recorded state.
        assert_eq!(
            read_component_activation(None, Some(&system), "net"),
            ActivationState::Stopped
        );
        // Path-shaped names never escape the root.
        assert_eq!(
            read_component_activation(Some(&user), Some(&system), "../net"),
            ActivationState::Installed
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn state_root_resolves_from_xdg_then_home() {
        assert_eq!(
            component_state_root_for(Some("override"), Some("xdg-state"), Some("home")),
            Some(PathBuf::from("override"))
        );
        assert_eq!(
            component_state_root_for(None, Some("xdg-state"), Some("home")),
            Some(PathBuf::from("xdg-state/bitty/extensions"))
        );
        assert_eq!(
            component_state_root_for(Some("  "), Some(" "), Some("home")),
            Some(PathBuf::from("home/.local/state/bitty/extensions"))
        );
        assert_eq!(component_state_root_for(None, Some(" "), Some(" ")), None);
    }

    #[test]
    fn state_record_covers_system_tier_and_survives_reinstall() {
        let base = scratch("state-tier");
        let system = base.join("system");
        let state = base.join("state");
        std::fs::create_dir_all(system.join("net")).expect("system dir");
        // No legacy record exists; the state directory wins and is created
        // on first write, including for a system-only install.
        assert_eq!(
            read_component_activation_with_state(None, Some(&system), Some(&state), "net"),
            ActivationState::Installed
        );
        let component_state = state.join("net");
        write_activation(&component_state, ActivationState::Active).expect("state record");
        assert_eq!(
            read_component_activation_with_state(None, Some(&system), Some(&state), "net"),
            ActivationState::Active
        );
        // A reinstalled component keeps the last transition until the next
        // activation overwrites it.
        assert_eq!(
            read_component_activation_with_state(None, None, Some(&state), "net"),
            ActivationState::Active
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_state_component_dir_is_refused() {
        let base = scratch("state-symlink-dir");
        let outside = base.join("outside");
        let state = base.join("state");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::create_dir_all(&state).expect("state");
        std::os::unix::fs::symlink(&outside, state.join("net")).expect("symlink");
        assert!(matches!(
            write_activation(&state.join("net"), ActivationState::Active),
            Err(ActivationWriteError::ComponentDirNotRegular)
        ));
        assert!(!outside.join(COMPONENT_ACTIVATION_FILE).exists());
        // Reading through the symlinked directory never claims a state.
        std::fs::write(outside.join(COMPONENT_ACTIVATION_FILE), "v1 active\n")
            .expect("outside record");
        assert_eq!(
            read_activation(&state.join("net")),
            ActivationState::Installed
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn stale_temp_removal_sweeps_only_activation_artifacts() {
        let dir = scratch("sweep");
        std::fs::write(activation_path(&dir), "v1 active\n").expect("record");
        std::fs::write(dir.join(format!("{ACTIVATION_TEMP_PREFIX}7-0")), "partial").expect("temp");
        std::fs::write(dir.join(format!("{ACTIVATION_TEMP_PREFIX}7-1")), "partial").expect("temp");
        std::fs::create_dir(dir.join(format!("{ACTIVATION_TEMP_PREFIX}8-0"))).expect("temp dir");
        std::fs::write(dir.join("unrelated"), "keep").expect("unrelated");
        let outside = dir.join("outside");
        std::fs::write(&outside, "v1 active\n").expect("outside");
        std::os::unix::fs::symlink(&outside, dir.join(format!("{ACTIVATION_TEMP_PREFIX}9-0")))
            .expect("temp link");
        remove_activation_artifacts(&dir);
        assert_eq!(read_activation(&dir), ActivationState::Installed);
        assert!(!dir.join(format!("{ACTIVATION_TEMP_PREFIX}7-0")).exists());
        assert!(!dir.join(format!("{ACTIVATION_TEMP_PREFIX}7-1")).exists());
        assert!(!dir.join(format!("{ACTIVATION_TEMP_PREFIX}8-0")).exists());
        assert!(!dir.join(format!("{ACTIVATION_TEMP_PREFIX}9-0")).exists());
        assert!(dir.join("unrelated").exists());
        assert_eq!(
            std::fs::read_to_string(&outside).expect("outside"),
            "v1 active\n",
            "sweeping a temp link must not follow it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
