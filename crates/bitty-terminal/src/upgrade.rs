//! `bitty upgrade`: install-source detection and self-update (bitty#2925).
//!
//! Local-class subcommand per the accepted CLI contract: it needs no running
//! instance, never loads a plugin VM, and works under `--safe` for the
//! read-only paths. It removes the re-run-the-`curl`-pipeline step for
//! script-installed users without ever fighting a package manager and without
//! ever elevating.
//!
//! # Channels
//!
//! Detection is read-only and ordered (first match wins):
//!
//! | Channel | Evidence | Scope | Action |
//! | --- | --- | --- | --- |
//! | `script` | install marker `channel = "script"` whose `bin_dir` holds the running binary | user | download, verify, atomic replace, keep `bitty.prev` |
//! | `appimage` | `$APPIMAGE` names the running executable | user | same, replacing the AppImage file |
//! | `homebrew` | running binary under `$(HOMEBREW_CELLAR)/bitty` and `brew` on `PATH` | user | `brew upgrade bitty` with `--yes` |
//! | `scoop` | running binary under `$SCOOP/apps/bitty` and `scoop` on `PATH` | user | `scoop update bitty` with `--yes` |
//! | `aur` | `pacman -Qo` owns the binary but `pacman -Si` cannot find the package | user | `yay`/`paru -S --noconfirm bitty` with `--yes` |
//! | `nix` | running binary under the Nix store | system | print `nix profile upgrade bitty`, exit `7` |
//! | `apt` | `dpkg-query -S` owns the binary | system | print `apt-get ... --only-upgrade bitty`, exit `7` |
//! | `dnf` / `rpm` | `rpm -qf` owns the binary (`dnf` present or not) | system | print the upgrade command, exit `7` |
//! | `pacman` | `pacman -Qo` owns the binary and `pacman -Si` finds the package | system | print `pacman -Syu bitty`, exit `7` |
//! | `unknown` | none of the above | — | print manual commands and the docs link, exit `1` |
//!
//! Windows self-managed installs cannot replace a running `.exe`, so the
//! script channel prints the installer command and exits `7` instead of
//! pretending to upgrade (in-place Windows replacement is deferred).
//!
//! # Exit codes (stable taxonomy)
//!
//! - `0` success (including "already current" and a printed plan/command).
//! - `1` `--check` found an update, or a fetch/verify/replace failure, or an
//!   undetectable install source.
//! - `2` usage error (contradictory flags, stray positional).
//! - `7` permission class: system-owned install, safe-mode write refusal, or
//!   a Windows self-managed install that needs the installer to re-run.
//!
//! # Bounds and safety
//!
//! Every fetch runs through the system `curl` with a fixed argv (no shell, no
//! interpolation): `--proto =https` (only a loopback test base may be plain
//! HTTP), `--proto-redir =https` (a redirect to a non-HTTPS URL is refused),
//! `--connect-timeout`, `--max-time`, `--max-redirs`, and `--max-filesize`.
//! The response is bounded again after the fact before any byte is parsed.
//! A download is verified against the release `SHA256SUMS` before the replace,
//! staged in the install directory itself, and swapped with `rename(2)` so the
//! previous binary survives as `bitty.prev`. Secret-shaped environment
//! variables never reach the child. No `unsafe`, no elevation, and no install
//! write outside the detected install directory (fetch staging goes to the
//! system temp directory and is removed on every path).
//!
//! # Trust model
//!
//! Verification is hash-only: `SHA256SUMS` is fetched from the same origin as
//! the asset, so it protects against a truncated or corrupted transfer, not
//! against a compromised CDN. A signed manifest is the follow-up; until it
//! lands the documented trust anchor is TLS plus the release workflow's own
//! `sha256sum -c` gate. `--dry-run` never downloads.

#![forbid(unsafe_code)]

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest as _, Sha256};

use crate::cli::Args;
use crate::doctor::json_escape;
use crate::version::version_semver;

// ---------------------------------------------------------------------------
// Exit codes (stable taxonomy, cli-contract-rfc.md)
// ---------------------------------------------------------------------------

/// Success: the requested action completed, or nothing had to change.
pub const EXIT_OK: i32 = 0;
/// `--check` found a newer published release (mirrors `diff`'s contract).
pub const EXIT_UPDATE_AVAILABLE: i32 = 1;
/// Generic failure: fetch, verification, replace, or an unknown channel.
pub const EXIT_GENERIC: i32 = 1;
/// CLI usage error (contradictory flags, stray positional).
pub const EXIT_USAGE: i32 = 2;
/// Permission class: system-owned install, safe-mode refusal, Windows
/// self-managed install.
pub const EXIT_PERM: i32 = 7;

// ---------------------------------------------------------------------------
// Bounds and stable locations
// ---------------------------------------------------------------------------

/// Default release host (mirrored from GitHub Releases).
pub const CDN_DEFAULT: &str = "https://cdn.bitty.run";

/// Mutable stable pointer holding `vX.Y.Z` (published by the release job).
pub const LATEST_POINTER_SUFFIX: &str = "install/latest.txt";

/// Per-channel upgrade documentation printed by the unknown-channel refusal.
pub const UPGRADE_DOCS_URL: &str =
    "https://github.com/bitty-terminal/bitty/blob/main/packaging/upgrade.md";

/// Connect timeout for every `curl` fetch (`--connect-timeout`).
pub const CONNECT_TIMEOUT_SECS: u64 = 15;
/// Total transfer timeout for every `curl` fetch (`--max-time`).
pub const MAX_TIME_SECS: u64 = 180;
/// Redirect hops allowed for a fetch (`--max-redirs`).
pub const MAX_REDIRECTS: u64 = 3;
/// Byte bound for the `latest.txt` pointer.
pub const LATEST_POINTER_MAX_BYTES: u64 = 4096;
/// Byte bound for `SHA256SUMS`.
pub const SHA256SUMS_MAX_BYTES: u64 = 1 << 20;
/// Byte bound for one release asset (binaries are ~40 MiB; 256 MiB is slack).
pub const ASSET_MAX_BYTES: u64 = 256 << 20;
/// Byte bound for the install marker (it is a tiny flat object).
pub const MARKER_MAX_BYTES: u64 = 64 << 10;
/// Timeout for one local `--version` probe (backup verification).
pub const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Timeout for one package-manager ownership query.
pub const OWNER_QUERY_TIMEOUT: Duration = Duration::from_secs(5);
/// Install-marker schema version written by `install.sh` / `install.ps1`.
pub const MARKER_SCHEMA: u32 = 1;

// ---------------------------------------------------------------------------
// Failure
// ---------------------------------------------------------------------------

/// One actionable failure: a stable exit code plus a one-line message.
#[derive(Debug)]
struct Failure {
    code: i32,
    message: String,
}

impl Failure {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn usage(message: impl Into<String>) -> Self {
        Self::new(EXIT_USAGE, message)
    }
}

/// Exit code of a completed action.
type Outcome = Result<i32, Failure>;

/// Maps an I/O error to a generic failure naming the operation's path.
fn io_failure(context: &str, error: &std::io::Error) -> Failure {
    Failure::new(EXIT_GENERIC, format!("{context}: {error}"))
}

// ---------------------------------------------------------------------------
// Versions
// ---------------------------------------------------------------------------

/// A stable `major.minor.patch` release version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Semver {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Semver {
    /// Parses `X.Y.Z` or `vX.Y.Z`; prereleases and suffixes are rejected
    /// (the stable channels never serve them).
    fn parse(raw: &str) -> Option<Self> {
        let bare = raw.trim();
        let bare = bare.strip_prefix('v').unwrap_or(bare);
        if bare.is_empty() || bare.contains('-') || bare.contains('+') {
            return None;
        }
        let mut parts = bare.split('.');
        let major = parse_version_part(parts.next()?)?;
        let minor = parse_version_part(parts.next()?)?;
        let patch = parse_version_part(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }

    /// Parses a release tag (`vX.Y.Z`) as published in `latest.txt`.
    fn parse_tag(raw: &str) -> Option<Self> {
        raw.strip_prefix('v')?;
        Self::parse(raw)
    }
}

/// One all-digit version part; empty or overflowing parts fail closed.
fn parse_version_part(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse::<u64>().ok()
}

impl std::fmt::Display for Semver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The running build's bare semver (`CARGO_PKG_VERSION`).
fn running_semver() -> Result<Semver, Failure> {
    Semver::parse(version_semver()).ok_or_else(|| {
        Failure::new(
            EXIT_GENERIC,
            format!(
                "this build reports version {:?}, which is not a stable X.Y.Z release; upgrade is unavailable",
                version_semver()
            ),
        )
    })
}

// ---------------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------------

/// Detected install source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// `install.sh` / `install.ps1` install recorded by the install marker.
    Script,
    /// AppImage invoked through `$APPIMAGE`.
    AppImage,
    /// Homebrew (user-scoped).
    Homebrew,
    /// Scoop (user-scoped, Windows).
    Scoop,
    /// AUR package built locally (user-scoped helper).
    Aur,
    /// Arch repository package (system-owned).
    Pacman,
    /// Debian/Ubuntu package (system-owned).
    Apt,
    /// Fedora/RHEL package with `dnf` (system-owned).
    Dnf,
    /// rpm-managed install without `dnf` (system-owned).
    Rpm,
    /// Nix store path (system-owned).
    Nix,
    /// Undetectable.
    Unknown,
}

impl Channel {
    /// Stable token used in output and tests.
    fn token(self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::AppImage => "appimage",
            Self::Homebrew => "homebrew",
            Self::Scoop => "scoop",
            Self::Aur => "aur",
            Self::Pacman => "pacman",
            Self::Apt => "apt",
            Self::Dnf => "dnf",
            Self::Rpm => "rpm",
            Self::Nix => "nix",
            Self::Unknown => "unknown",
        }
    }

    /// Self-managed channels that `bitty upgrade` can replace in place.
    fn is_self_managed(self) -> bool {
        matches!(self, Self::Script | Self::AppImage)
    }

    /// Channels owned by the system package manager: never executed.
    fn is_system_owned(self) -> bool {
        matches!(
            self,
            Self::Pacman | Self::Apt | Self::Dnf | Self::Rpm | Self::Nix
        )
    }

    /// Delegated command printed for the channel (never contains a secret).
    fn delegated_command(self, helper: Option<&str>) -> Option<String> {
        match self {
            Self::Homebrew => Some("brew upgrade bitty".to_string()),
            Self::Scoop => Some("scoop update bitty".to_string()),
            Self::Aur => Some(match helper {
                Some(helper) => format!("{helper} -S --noconfirm bitty"),
                None => "yay -S --noconfirm bitty".to_string(),
            }),
            Self::Pacman => Some("sudo pacman -Syu bitty".to_string()),
            Self::Apt => {
                Some("sudo apt-get update && sudo apt-get install --only-upgrade bitty".to_string())
            }
            Self::Dnf => Some("sudo dnf upgrade bitty".to_string()),
            Self::Rpm => Some("sudo rpm -U bitty-<version>-1.<arch>.rpm".to_string()),
            Self::Nix => Some("nix profile upgrade bitty".to_string()),
            Self::Script | Self::AppImage | Self::Unknown => None,
        }
    }

    /// Fixed argv for the user-scoped delegated channels (`--yes` only).
    fn delegated_argv(self, helper: Option<&str>) -> Option<Vec<String>> {
        match self {
            Self::Homebrew => Some(vec!["brew".into(), "upgrade".into(), "bitty".into()]),
            Self::Scoop => Some(vec!["scoop".into(), "update".into(), "bitty".into()]),
            Self::Aur => Some(vec![
                helper.unwrap_or("yay").to_string(),
                "-S".into(),
                "--noconfirm".into(),
                "bitty".into(),
            ]),
            _ => None,
        }
    }
}

/// Where the running binary lives (self-managed channels only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// Running binary (or AppImage) path.
    binary: PathBuf,
    /// Directory that contains it (the only directory upgrade writes to).
    bin_dir: PathBuf,
}

impl Location {
    /// Derives the location from the binary path.
    fn for_binary(binary: PathBuf) -> Self {
        let bin_dir = binary
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        Self { binary, bin_dir }
    }
}

/// One detection result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    channel: Channel,
    /// Human evidence naming the marker/env/query that matched.
    evidence: String,
    /// Self-managed install location (absent for delegated channels).
    location: Option<Location>,
    /// AUR helper found on `PATH` (`yay`/`paru`).
    helper: Option<String>,
}

// ---------------------------------------------------------------------------
// Probe (injectable environment)
// ---------------------------------------------------------------------------

/// Everything detection reads: the running executable, the `PATH` search
/// directories, and an environment lookup. Injectable so the detection table
/// is provable with stub tools and a fake `PATH` (no process-environment
/// mutation, no real package manager).
pub struct Probe<'a> {
    /// Running executable path (`current_exe` in production).
    pub exe: PathBuf,
    /// `PATH` directories, in order.
    pub path_dirs: Vec<PathBuf>,
    /// Environment lookup (`None` when unset or empty).
    pub env: &'a dyn Fn(&str) -> Option<String>,
}

impl Probe<'static> {
    /// Production probe: `current_exe`, `PATH`, and the process environment.
    fn from_process() -> Self {
        let path_dirs = std::env::var_os("PATH")
            .map(|value| std::env::split_paths(&value).collect())
            .unwrap_or_default();
        Self {
            exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("bitty")),
            path_dirs,
            env: &process_env,
        }
    }
}

impl Probe<'_> {
    fn env_value(&self, key: &str) -> Option<String> {
        (self.env)(key).filter(|value| !value.is_empty())
    }

    /// First executable named `tool` on the probe's `PATH`.
    fn which(&self, tool: &str) -> Option<PathBuf> {
        which_in(tool, &self.path_dirs)
    }
}

/// Process-environment lookup (the production [`Probe::env`]).
fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// `PATH` search: first regular executable file named `tool`.
fn which_in(tool: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{tool}.exe"), tool.to_string()]
    } else {
        vec![tool.to_string()]
    };
    for dir in dirs {
        for name in &names {
            let candidate = dir.join(name);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// Detects the install source from read-only evidence.
fn detect(probe: &Probe<'_>) -> Detection {
    detect_script(probe)
        .or_else(|| detect_appimage(probe))
        .or_else(|| detect_homebrew(probe))
        .or_else(|| detect_scoop(probe))
        .or_else(|| detect_nix(probe))
        .or_else(|| detect_dpkg(probe))
        .or_else(|| detect_rpm(probe))
        .or_else(|| detect_pacman(probe))
        .unwrap_or_else(|| Detection {
            channel: Channel::Unknown,
            evidence: "no install marker, $APPIMAGE, or package-manager owner matched".to_string(),
            location: None,
            helper: None,
        })
}

/// Platform binary file name inside a bin directory.
fn binary_file_name() -> &'static str {
    if cfg!(windows) { "bitty.exe" } else { "bitty" }
}

/// `script` channel: the install marker names this binary's directory.
fn detect_script(probe: &Probe<'_>) -> Option<Detection> {
    let marker_path = marker_path(probe.env)?;
    let marker = Marker::read(&marker_path).ok()?;
    if marker.channel != "script" {
        return None;
    }
    let binary = PathBuf::from(&marker.bin_dir).join(binary_file_name());
    if !same_path(&binary, &probe.exe) {
        return None;
    }
    Some(Detection {
        channel: Channel::Script,
        evidence: format!("install marker {}", marker_path.display()),
        location: Some(Location::for_binary(binary)),
        helper: None,
    })
}

/// `appimage` channel: `$APPIMAGE` is the running file.
fn detect_appimage(probe: &Probe<'_>) -> Option<Detection> {
    let appimage = PathBuf::from(probe.env_value("APPIMAGE")?);
    if !same_path(&appimage, &probe.exe) {
        return None;
    }
    Some(Detection {
        channel: Channel::AppImage,
        evidence: "$APPIMAGE".to_string(),
        location: Some(Location::for_binary(appimage)),
        helper: None,
    })
}

/// Homebrew cellars to check, most specific first.
fn homebrew_cellars(probe: &Probe<'_>) -> Vec<PathBuf> {
    let mut cellars = Vec::new();
    if let Some(cellar) = probe.env_value("HOMEBREW_CELLAR") {
        cellars.push(PathBuf::from(cellar));
    }
    if let Some(prefix) = probe.env_value("HOMEBREW_PREFIX") {
        cellars.push(PathBuf::from(prefix).join("Cellar"));
    }
    // Conventional prefixes: the same OS layout the formula itself uses, not
    // a developer path.
    cellars.push(PathBuf::from("/opt/homebrew/Cellar"));
    cellars.push(PathBuf::from("/usr/local/Cellar"));
    cellars
}

/// `homebrew` channel: running binary lives in the Homebrew cellar.
fn detect_homebrew(probe: &Probe<'_>) -> Option<Detection> {
    probe.which("brew")?;
    let root = homebrew_cellars(probe)
        .into_iter()
        .map(|cellar| cellar.join("bitty"))
        .find(|root| path_starts_with(&probe.exe, root))?;
    Some(Detection {
        channel: Channel::Homebrew,
        evidence: format!("binary under {}", root.display()),
        location: None,
        helper: None,
    })
}

/// `scoop` channel: running binary lives in the Scoop app directory.
fn detect_scoop(probe: &Probe<'_>) -> Option<Detection> {
    probe.which("scoop")?;
    let mut roots = Vec::new();
    if let Some(root) = probe.env_value("SCOOP") {
        roots.push(PathBuf::from(root));
    }
    if let Some(profile) = probe.env_value("USERPROFILE") {
        roots.push(PathBuf::from(profile).join("scoop"));
    }
    let root = roots
        .into_iter()
        .map(|root| root.join("apps").join("bitty"))
        .find(|root| path_starts_with(&probe.exe, root))?;
    Some(Detection {
        channel: Channel::Scoop,
        evidence: format!("binary under {}", root.display()),
        location: None,
        helper: None,
    })
}

/// `nix` channel: the Nix store is an unambiguous install source.
fn detect_nix(probe: &Probe<'_>) -> Option<Detection> {
    let store = probe
        .env_value("NIX_STORE_DIR")
        .map_or_else(|| PathBuf::from("/nix/store"), PathBuf::from);
    if !path_starts_with(&probe.exe, &store) {
        return None;
    }
    Some(Detection {
        channel: Channel::Nix,
        evidence: format!("binary under {}", store.display()),
        location: None,
        helper: None,
    })
}

/// `apt` channel: `dpkg-query -S` owns the running binary.
fn detect_dpkg(probe: &Probe<'_>) -> Option<Detection> {
    let tool = probe.which("dpkg-query")?;
    let exe = probe.exe.to_string_lossy();
    let output = run_bounded(
        Command::new(&tool).args(["-S", exe.as_ref()]),
        OWNER_QUERY_TIMEOUT,
    )?;
    if !output.status.success() {
        return None;
    }
    Some(Detection {
        channel: Channel::Apt,
        evidence: "dpkg-query -S owns the binary".to_string(),
        location: None,
        helper: None,
    })
}

/// `rpm`-family channels: `rpm -qf` owns the binary; `dnf` decides the verb.
fn detect_rpm(probe: &Probe<'_>) -> Option<Detection> {
    let tool = probe.which("rpm")?;
    let exe = probe.exe.to_string_lossy();
    let output = run_bounded(
        Command::new(&tool).args(["-qf", exe.as_ref()]),
        OWNER_QUERY_TIMEOUT,
    )?;
    if !output.status.success() {
        return None;
    }
    let channel = if probe.which("dnf").is_some() {
        Channel::Dnf
    } else {
        Channel::Rpm
    };
    Some(Detection {
        channel,
        evidence: "rpm -qf owns the binary".to_string(),
        location: None,
        helper: None,
    })
}

/// `pacman` / `aur` channels: `pacman -Qo` owns the binary; `pacman -Si`
/// decides whether the package came from a configured repository.
fn detect_pacman(probe: &Probe<'_>) -> Option<Detection> {
    let tool = probe.which("pacman")?;
    let exe = probe.exe.to_string_lossy();
    let output = run_bounded(
        Command::new(&tool).args(["-Qo", exe.as_ref()]),
        OWNER_QUERY_TIMEOUT,
    )?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let package = package_from_pacman_query(&stdout)?;
    let in_repo = run_bounded(
        Command::new(&tool).args(["-Si", package.as_str()]),
        OWNER_QUERY_TIMEOUT,
    )
    .is_some_and(|probe| probe.status.success());
    if in_repo {
        return Some(Detection {
            channel: Channel::Pacman,
            evidence: format!(
                "pacman -Qo owns the binary ({package} is in a configured repository)"
            ),
            location: None,
            helper: None,
        });
    }
    let helper = ["yay", "paru"]
        .into_iter()
        .find(|candidate| probe.which(candidate).is_some())
        .map(str::to_string);
    Some(Detection {
        channel: Channel::Aur,
        evidence: format!(
            "pacman -Qo owns the binary ({package} is not in a configured repository)"
        ),
        location: None,
        helper,
    })
}

/// Parses `pacman -Qo` output (`/usr/bin/bitty is owned by bitty 0.0.27-1`).
fn package_from_pacman_query(stdout: &str) -> Option<String> {
    let (_, rest) = stdout.split_once(" is owned by ")?;
    rest.split_whitespace().next().map(str::to_string)
}

/// Component-prefix test (`path` is `root` or below it), lexical on purpose:
/// the Nix store, cellar, and Scoop roots are compared without touching disk.
fn path_starts_with(path: &Path, root: &Path) -> bool {
    let mut path_components = path.components();
    for root_component in root.components() {
        match path_components.next() {
            Some(component) if component == root_component => {}
            _ => return false,
        }
    }
    true
}

/// Whether two paths name the same file (canonical when both exist).
fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Install marker
// ---------------------------------------------------------------------------

/// Marker path: `%LOCALAPPDATA%\bitty\install.json` on Windows,
/// `$XDG_DATA_HOME/bitty/install.json` (`~/.local/share` fallback) elsewhere.
fn marker_path(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let base = if cfg!(windows) {
        PathBuf::from(env("LOCALAPPDATA")?)
    } else {
        match env("XDG_DATA_HOME") {
            Some(data_home) if Path::new(&data_home).is_absolute() => PathBuf::from(data_home),
            _ => PathBuf::from(env("HOME")?).join(".local").join("share"),
        }
    };
    Some(base.join("bitty").join("install.json"))
}

/// One install marker (written by `install.sh` / `install.ps1`, refreshed by
/// `bitty upgrade`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// `script` today; the field keeps the marker channel-agnostic.
    channel: String,
    /// Install prefix the script was given.
    prefix: String,
    /// Directory holding the installed binary.
    bin_dir: String,
    /// Installed version (bare `X.Y.Z`).
    version: String,
    /// SHA-256 of the installed binary.
    binary_sha256: String,
    /// UTC timestamp of the last successful install or upgrade.
    installed_at: String,
}

impl Marker {
    /// Reads and parses a marker; every problem is an evidence string.
    fn read(path: &Path) -> Result<Self, String> {
        let metadata =
            std::fs::metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
        if !metadata.is_file() {
            return Err(format!("{}: not a regular file", path.display()));
        }
        if metadata.len() > MARKER_MAX_BYTES {
            return Err(format!(
                "{}: {} bytes exceeds the {MARKER_MAX_BYTES} byte marker bound",
                path.display(),
                metadata.len()
            ));
        }
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Self::parse(&text).ok_or_else(|| format!("{}: malformed marker", path.display()))
    }

    /// Strict parse of the flat marker object.
    fn parse(text: &str) -> Option<Self> {
        let fields = parse_flat_object(text)?;
        let mut channel = None;
        let mut prefix = None;
        let mut bin_dir = None;
        let mut version = None;
        let mut binary_sha256 = None;
        let mut installed_at = None;
        for (key, value) in fields {
            match key.as_str() {
                "channel" => channel = Some(value),
                "prefix" => prefix = Some(value),
                "bin_dir" => bin_dir = Some(value),
                "version" => version = Some(value),
                "binary_sha256" => binary_sha256 = Some(value),
                "installed_at" => installed_at = Some(value),
                _ => {}
            }
        }
        let marker = Self {
            channel: channel?,
            prefix: prefix?,
            bin_dir: bin_dir?,
            version: version?,
            binary_sha256: binary_sha256?,
            installed_at: installed_at?,
        };
        if marker.bin_dir.is_empty() || marker.channel.is_empty() {
            return None;
        }
        Some(marker)
    }

    /// Serializes the marker (pretty, stable key order).
    fn to_json(&self) -> String {
        format!(
            "{{\n  \"schema\": {MARKER_SCHEMA},\n  \"channel\": \"{}\",\n  \"prefix\": \"{}\",\n  \"bin_dir\": \"{}\",\n  \"version\": \"{}\",\n  \"binary_sha256\": \"{}\",\n  \"installed_at\": \"{}\"\n}}\n",
            json_escape(&self.channel),
            json_escape(&self.prefix),
            json_escape(&self.bin_dir),
            json_escape(&self.version),
            json_escape(&self.binary_sha256),
            json_escape(&self.installed_at),
        )
    }

    /// Rewrites the marker after a successful upgrade/rollback.
    fn refresh(&mut self, version: &Semver, binary_sha256: &str) {
        self.version = version.to_string();
        self.binary_sha256 = binary_sha256.to_string();
        self.installed_at = iso8601_utc(SystemTime::now());
    }

    /// Writes the marker atomically (staged in the same directory).
    fn write(&self, path: &Path) -> std::io::Result<()> {
        let Some(parent) = path.parent() else {
            return Err(std::io::Error::other("marker path has no parent"));
        };
        std::fs::create_dir_all(parent)?;
        let staged = parent.join(format!(".install.json.{}.tmp", std::process::id()));
        std::fs::write(&staged, self.to_json())?;
        let result = std::fs::rename(&staged, path);
        if result.is_err() {
            let _ = std::fs::remove_file(&staged);
        }
        result
    }
}

/// Refreshes the marker after a self-managed upgrade when it describes this
/// install directory. Best effort: a failure only warns.
fn refresh_marker(env: &dyn Fn(&str) -> Option<String>, location: &Location, version: &Semver) {
    let Some(path) = marker_path(env) else {
        return;
    };
    let Ok(mut marker) = Marker::read(&path) else {
        return;
    };
    if !same_path(Path::new(&marker.bin_dir), &location.bin_dir) {
        return;
    }
    let Ok(digest) = sha256_file(&location.binary) else {
        return;
    };
    marker.refresh(version, &digest);
    if let Err(error) = marker.write(&path) {
        eprintln!(
            "bitty upgrade: warning: could not refresh {}: {error}",
            path.display()
        );
    }
}

/// Strict parser for the flat JSON object the marker uses (string values,
/// `\uXXXX` escapes, no nesting). Returns `None` on any deviation.
fn parse_flat_object(text: &str) -> Option<Vec<(String, String)>> {
    let mut scanner = Scanner {
        bytes: text.as_bytes(),
        pos: 0,
    };
    scanner.skip_whitespace();
    scanner.expect(b'{')?;
    let mut fields = Vec::new();
    scanner.skip_whitespace();
    if scanner.peek() == Some(b'}') {
        scanner.bump();
    } else {
        loop {
            scanner.skip_whitespace();
            let key = scanner.string()?;
            scanner.skip_whitespace();
            scanner.expect(b':')?;
            scanner.skip_whitespace();
            let value = scanner.scalar()?;
            fields.push((key, value));
            scanner.skip_whitespace();
            match scanner.peek()? {
                b',' => {
                    scanner.bump();
                }
                b'}' => {
                    scanner.bump();
                    break;
                }
                _ => return None,
            }
        }
    }
    scanner.skip_whitespace();
    if scanner.pos != scanner.bytes.len() {
        return None;
    }
    Some(fields)
}

/// Byte scanner for [`parse_flat_object`].
struct Scanner<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Scanner<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) {
        self.pos += 1;
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.bump();
        }
    }

    fn expect(&mut self, byte: u8) -> Option<()> {
        if self.peek()? == byte {
            self.bump();
            Some(())
        } else {
            None
        }
    }

    fn string(&mut self) -> Option<String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let byte = self.peek()?;
            self.bump();
            match byte {
                b'"' => return Some(out),
                b'\\' => {
                    let escape = self.peek()?;
                    self.bump();
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let mut code = 0u32;
                            for _ in 0..4 {
                                let digit = self.peek()?;
                                self.bump();
                                code = code * 16 + (digit as char).to_digit(16)?;
                            }
                            out.push(char::from_u32(code)?);
                        }
                        _ => return None,
                    }
                }
                byte if byte < 0x20 => return None,
                byte => {
                    // The input is a &str, so a multi-byte UTF-8 sequence is
                    // copied verbatim from the original text.
                    let start = self.pos - 1;
                    let width = utf8_width(byte)?;
                    if start + width > self.bytes.len() {
                        return None;
                    }
                    out.push_str(std::str::from_utf8(&self.bytes[start..start + width]).ok()?);
                    self.pos = start + width;
                }
            }
        }
    }

    /// String, number, boolean, or null: the marker only reads strings, but
    /// tolerating other scalars keeps forward-compatible fields harmless.
    fn scalar(&mut self) -> Option<String> {
        match self.peek()? {
            b'"' => self.string(),
            _ => {
                let start = self.pos;
                while let Some(byte) = self.peek() {
                    if matches!(byte, b',' | b'}' | b' ' | b'\t' | b'\n' | b'\r') {
                        break;
                    }
                    self.bump();
                }
                if self.pos == start {
                    return None;
                }
                std::str::from_utf8(&self.bytes[start..self.pos])
                    .ok()
                    .map(str::to_string)
            }
        }
    }
}

/// UTF-8 sequence width from its leading byte.
fn utf8_width(byte: u8) -> Option<usize> {
    match byte {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------

/// `YYYY-MM-DDTHH:MM:SSZ` for one system time (UTC, no external crate).
fn iso8601_utc(now: SystemTime) -> String {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let days = (seconds / 86_400) as i64;
    let remainder = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        remainder / 3600,
        (remainder % 3600) / 60,
        remainder % 60
    )
}

/// Days since the Unix epoch to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

// ---------------------------------------------------------------------------
// Hashing
// ---------------------------------------------------------------------------

/// Lowercase hex SHA-256 of one file.
fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for index in 0..digest.len() {
        use std::fmt::Write as _;
        let _ = write!(hex, "{:02x}", digest[index]);
    }
    Ok(hex)
}

/// The digest `SHA256SUMS` records for exactly `asset` (`<hex>  <name>` or
/// `<hex> *<name>`). Ambiguous or malformed entries fail closed.
fn digest_for(sums: &str, asset: &str) -> Option<String> {
    let mut found = None;
    for line in sums.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let digest = fields.next()?;
        let name = fields.next()?.trim_start_matches('*');
        if fields.next().is_some() {
            return None;
        }
        if name != asset {
            continue;
        }
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        if found.is_some() {
            return None;
        }
        found = Some(digest.to_ascii_lowercase());
    }
    found
}

// ---------------------------------------------------------------------------
// Network (bounded system curl)
// ---------------------------------------------------------------------------

/// Release base URL: `BITTY_CDN_BASE` override (test/manual) or the CDN.
fn cdn_base(probe: &Probe<'_>) -> Result<String, Failure> {
    let raw = probe
        .env_value("BITTY_CDN_BASE")
        .unwrap_or_else(|| CDN_DEFAULT.to_string());
    let base = raw.trim_end_matches('/').to_string();
    let scheme = base.split_once("://").map(|(scheme, _)| scheme);
    match scheme {
        Some("https") => Ok(base),
        Some("http") if is_loopback_url(&base) => Ok(base),
        _ => Err(Failure::new(
            EXIT_GENERIC,
            format!(
                "BITTY_CDN_BASE={raw:?} is not an HTTPS URL (only a loopback test base may use HTTP)"
            ),
        )),
    }
}

/// Loopback check for the plain-HTTP test seam.
fn is_loopback_url(url: &str) -> bool {
    let Some((_, rest)) = url.split_once("://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.contains('@') {
        return false;
    }
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

/// Fixed `curl` argv for one fetch.
///
/// `--proto =https` unless the URL is a loopback test base; `--proto-redir
/// =https` always, so a redirect to a non-HTTPS URL is refused. No CA, proxy,
/// or TLS override: the system store and the inherited proxy environment stay
/// authoritative. `--write-out` returns the HTTP status on stdout (the body
/// goes to `dest`), so HTTP failures stay typed without `--fail`.
fn curl_argv(url: &str, dest: &Path, max_bytes: u64) -> Result<Vec<String>, Failure> {
    let proto = if url.starts_with("https://") {
        "=https"
    } else if url.starts_with("http://") && is_loopback_url(url) {
        "=https,http"
    } else {
        return Err(Failure::new(
            EXIT_GENERIC,
            format!("refusing to fetch a non-HTTPS URL: {url}"),
        ));
    };
    Ok(vec![
        "--silent".to_string(),
        "--show-error".to_string(),
        "--location".to_string(),
        "--globoff".to_string(),
        "--max-redirs".to_string(),
        MAX_REDIRECTS.to_string(),
        "--proto".to_string(),
        proto.to_string(),
        "--proto-redir".to_string(),
        "=https".to_string(),
        "--connect-timeout".to_string(),
        CONNECT_TIMEOUT_SECS.to_string(),
        "--max-time".to_string(),
        MAX_TIME_SECS.to_string(),
        "--max-filesize".to_string(),
        max_bytes.to_string(),
        "--write-out".to_string(),
        "%{http_code}".to_string(),
        "--output".to_string(),
        dest.to_string_lossy().into_owned(),
        url.to_string(),
    ])
}

/// Runs one bounded `curl` fetch; returns the HTTP status code.
fn run_curl(url: &str, dest: &Path, max_bytes: u64) -> Result<u16, Failure> {
    let argv = curl_argv(url, dest, max_bytes)?;
    let mut command = Command::new("curl");
    command.args(&argv);
    scrub_secret_env(&mut command);
    let output = command.stdin(Stdio::null()).output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Failure::new(
                EXIT_GENERIC,
                "`curl` not found on PATH; install curl to use `bitty upgrade`",
            )
        } else {
            Failure::new(EXIT_GENERIC, format!("cannot run `curl`: {error}"))
        }
    })?;
    let status_code = output.status.code();
    let http_status = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u16>()
        .unwrap_or(0);
    if status_code != Some(0) {
        return Err(classify_curl_failure(&output, max_bytes));
    }
    Ok(http_status)
}

/// Typed refusal for a failed `curl` transfer (raw stderr is never echoed).
fn classify_curl_failure(output: &Output, max_bytes: u64) -> Failure {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = match output.status.code() {
        Some(63) => format!(
            "refused: the response exceeds the {max_bytes} byte bound (curl --max-filesize); nothing was written"
        ),
        Some(28) => "the release host did not answer within the transfer timeout".to_string(),
        Some(60) => "the release host's TLS certificate could not be verified".to_string(),
        Some(6) => "the release host could not be resolved".to_string(),
        Some(7) => "the release host refused the connection".to_string(),
        Some(23) => "cannot write the download to disk".to_string(),
        _ if stderr.contains("(in redirect)") => {
            "refused: the release host redirected to a non-HTTPS URL".to_string()
        }
        _ if stderr.contains("Protocol") && stderr.contains("disabled") => {
            "refused: the release host redirected to a non-HTTPS URL".to_string()
        }
        Some(code) => format!("the release fetch failed (curl exit {code})"),
        None => "the release fetch was terminated before it completed".to_string(),
    };
    Failure::new(EXIT_GENERIC, message)
}

/// Removes secret-shaped variables from the fetch child (proxy and CA
/// discovery keep working; no secret crosses into `curl`).
fn scrub_secret_env(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        let Ok(name) = name.into_string() else {
            continue;
        };
        let upper = name.to_ascii_uppercase();
        if upper.ends_with("_TOKEN") || upper.ends_with("_KEY") || upper.ends_with("_SECRET") {
            command.env_remove(&name);
        }
    }
}

/// Requires a 200 response; other statuses are typed refusals.
fn require_http_ok(url: &str, status: u16) -> Result<(), Failure> {
    if status == 200 {
        return Ok(());
    }
    let detail = match status {
        0 => "no HTTP status (the transfer did not complete)".to_string(),
        404 => "404: the object is not published".to_string(),
        401 | 403 => format!("{status}: access denied"),
        _ => format!("HTTP {status}"),
    };
    Err(Failure::new(EXIT_GENERIC, format!("{url}: {detail}")))
}

/// One bounded text fetch into a scratch file under the system temp dir.
fn fetch_text(
    probe: &Probe<'_>,
    base: &str,
    suffix: &str,
    max_bytes: u64,
) -> Result<String, Failure> {
    let url = format!("{base}/{suffix}");
    let scratch = scratch_path(probe, "text");
    let result = (|| {
        let status = run_curl(&url, &scratch, max_bytes)?;
        require_http_ok(&url, status)?;
        let bytes = read_capped(&scratch, max_bytes)?;
        String::from_utf8(bytes)
            .map_err(|_| Failure::new(EXIT_GENERIC, format!("{url}: response is not UTF-8")))
    })();
    let _ = std::fs::remove_file(&scratch);
    result
}

/// Unique scratch path in the system temp dir (fetch staging only).
fn scratch_path(probe: &Probe<'_>, tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let base = probe
        .env_value("TMPDIR")
        .map_or_else(std::env::temp_dir, PathBuf::from);
    base.join(format!(
        "bitty-upgrade-{tag}-{}-{sequence}.tmp",
        std::process::id()
    ))
}

/// Reads at most `max_bytes`, refusing anything larger.
fn read_capped(path: &Path, max_bytes: u64) -> Result<Vec<u8>, Failure> {
    let metadata =
        std::fs::metadata(path).map_err(|error| io_failure(&path.display().to_string(), &error))?;
    if metadata.len() > max_bytes {
        return Err(Failure::new(
            EXIT_GENERIC,
            format!(
                "{}: {} bytes exceeds the {max_bytes} byte bound; refusing to read it",
                path.display(),
                metadata.len()
            ),
        ));
    }
    let file = std::fs::File::open(path)
        .map_err(|error| io_failure(&path.display().to_string(), &error))?;
    let mut bytes = Vec::new();
    file.take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io_failure(&path.display().to_string(), &error))?;
    if bytes.len() as u64 > max_bytes {
        return Err(Failure::new(
            EXIT_GENERIC,
            format!(
                "{}: response exceeds the {max_bytes} byte bound; refusing to read it",
                path.display()
            ),
        ));
    }
    Ok(bytes)
}

/// Fetches and validates the published `vX.Y.Z` pointer.
fn fetch_latest_tag(probe: &Probe<'_>, base: &str) -> Result<String, Failure> {
    let raw = fetch_text(probe, base, LATEST_POINTER_SUFFIX, LATEST_POINTER_MAX_BYTES)?;
    let tag = raw.trim();
    if Semver::parse_tag(tag).is_none() {
        return Err(Failure::new(
            EXIT_GENERIC,
            format!(
                "{base}/{LATEST_POINTER_SUFFIX} returned {tag:?}, which is not a stable vX.Y.Z tag"
            ),
        ));
    }
    Ok(tag.to_string())
}

// ---------------------------------------------------------------------------
// Release assets
// ---------------------------------------------------------------------------

/// Rust target triple of this build (the release layout names assets after it).
fn target_triple() -> Result<&'static str, Failure> {
    let triple = if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) {
        "x86_64-unknown-linux-musl"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "aarch64-pc-windows-msvc"
    } else {
        return Err(Failure::new(
            EXIT_GENERIC,
            "this platform has no published release asset; upgrade manually from the release page",
        ));
    };
    Ok(triple)
}

/// Release asset name for one channel (`SHA256SUMS` uses the same names).
fn asset_name(channel: Channel) -> Result<String, Failure> {
    let triple = target_triple()?;
    match channel {
        Channel::AppImage => Ok(format!("bitty-{triple}.AppImage")),
        _ => Ok(format!("bitty-{triple}")),
    }
}

/// `bitty` -> `bitty.prev`, `Bitty.AppImage` -> `Bitty.AppImage.prev`.
fn prev_path(binary: &Path) -> Result<PathBuf, Failure> {
    let name = binary.file_name().ok_or_else(|| {
        Failure::new(
            EXIT_GENERIC,
            format!("{} has no file name", binary.display()),
        )
    })?;
    let mut prev = name.to_os_string();
    prev.push(".prev");
    Ok(binary.with_file_name(prev))
}

/// Staged download path in the install directory (same filesystem, so the
/// final `rename(2)` is atomic).
fn staged_path(bin_dir: &Path) -> PathBuf {
    bin_dir.join(format!(".bitty-upgrade.{}.tmp", std::process::id()))
}

#[cfg(unix)]
fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Replaces `binary` with `staged`, keeping the previous bytes as `prev`.
///
/// The previous binary is copied first (so a crash leaves a working
/// `binary`), then the staged file is renamed over it in the same directory.
fn install_binary(staged: &Path, binary: &Path, prev: &Path) -> Result<(), Failure> {
    if !binary.is_file() {
        return Err(Failure::new(
            EXIT_GENERIC,
            format!(
                "{} is not a regular file; refusing to replace it",
                binary.display()
            ),
        ));
    }
    if staged.parent() != binary.parent() {
        return Err(Failure::new(
            EXIT_GENERIC,
            format!(
                "refusing to install outside {}: {}",
                binary.parent().unwrap_or(Path::new(".")).display(),
                staged.display()
            ),
        ));
    }
    std::fs::copy(binary, prev).map_err(|error| {
        io_failure(
            &format!("cannot back up {} to {}", binary.display(), prev.display()),
            &error,
        )
    })?;
    if let Ok(handle) = std::fs::File::open(prev) {
        let _ = handle.sync_all();
    }
    std::fs::rename(staged, binary).map_err(|error| {
        io_failure(
            &format!(
                "cannot replace {} with {}",
                binary.display(),
                staged.display()
            ),
            &error,
        )
    })
}

/// Runs `<binary> --version` under [`VERSION_PROBE_TIMEOUT`] and returns the
/// bare semver it reports.
fn probe_version(binary: &Path) -> Result<Semver, Failure> {
    let output = run_bounded(Command::new(binary).arg("--version"), VERSION_PROBE_TIMEOUT)
        .ok_or_else(|| {
            Failure::new(
                EXIT_GENERIC,
                format!("{} did not answer `--version`", binary.display()),
            )
        })?;
    if !output.status.success() {
        return Err(Failure::new(
            EXIT_GENERIC,
            format!("{} --version failed ({})", binary.display(), output.status),
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    version_from_output(&text).ok_or_else(|| {
        Failure::new(
            EXIT_GENERIC,
            format!(
                "{} --version did not report a bitty version",
                binary.display()
            ),
        )
    })
}

/// Parses `bitty X.Y.Z (...)` from `--version` output.
fn version_from_output(text: &str) -> Option<Semver> {
    let token = text.split_whitespace().nth(1)?;
    Semver::parse(token)
}

/// Spawns a command with a hard deadline; `None` when it cannot start or
/// exceeds the deadline (killed by PID, never by pattern).
fn run_bounded(command: &mut Command, timeout: Duration) -> Option<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Short usage for `bitty upgrade` (help and fail-closed exit 2).
#[must_use]
pub fn upgrade_usage() -> String {
    "usage: bitty upgrade [--check | --rollback] [--dry-run] [--yes]\n\
     \n\
     \x20 --check     Read the published release tag and compare it with this\n\
     \x20             build (read-only; allowed under --safe). Exit 0 when\n\
     \x20             current, 1 when an update is available.\n\
     \x20 --dry-run   Detect the install source and print the plan; nothing is\n\
     \x20             downloaded or written.\n\
     \x20 --yes       Execute a delegated user-scoped command (Homebrew, Scoop,\n\
     \x20             AUR). Without it the command is printed only. System-owned\n\
     \x20             installs are never executed and exit 7.\n\
     \x20 --rollback  Restore bitty.prev (script/AppImage installs only) after\n\
     \x20             verifying that the backup reports a version.\n\
     \x20 -h, --help  Show this help.\n\
     \n\
     Script and AppImage installs download the matching release asset, verify\n\
     its sha256 against the release SHA256SUMS, and replace the binary\n\
     atomically (the previous binary stays as bitty.prev). Package-manager\n\
     installs are delegated, never fought: bitty prints the exact command and\n\
     never elevates. Upgrade docs: <https://github.com/bitty-terminal/bitty/blob/main/packaging/upgrade.md>."
        .to_string()
}

/// `bitty upgrade` entry point (dispatched before config load).
pub fn run_cli(args: &Args) -> i32 {
    let probe = Probe::from_process();
    match run(args, &probe) {
        Ok(code) => code,
        Err(failure) => {
            eprintln!("bitty upgrade: {}", failure.message);
            if failure.code == EXIT_USAGE {
                eprintln!("{}", upgrade_usage());
            }
            failure.code
        }
    }
}

fn run(args: &Args, probe: &Probe<'_>) -> Outcome {
    if let Some(extra) = args.upgrade_args.first() {
        return Err(Failure::usage(format!(
            "unexpected argument {extra:?}; `bitty upgrade` takes flags only"
        )));
    }
    let check = args.upgrade_check;
    let dry_run = args.upgrade_dry_run;
    let yes = args.upgrade_yes;
    let rollback = args.upgrade_rollback;

    if check && (rollback || yes || dry_run) {
        return Err(Failure::usage(
            "--check is read-only and cannot be combined with --rollback, --yes, or --dry-run",
        ));
    }
    if rollback && yes {
        return Err(Failure::usage(
            "--rollback takes no --yes: it never runs a package manager",
        ));
    }
    if args.safe && !check && !dry_run {
        return Err(Failure::new(
            EXIT_PERM,
            "safe mode is read-only: `bitty upgrade --check` reports versions and `bitty upgrade --dry-run` prints the plan; nothing is written or downloaded",
        ));
    }

    let base = cdn_base(probe)?;
    if check {
        return check_only(probe, &base);
    }

    let detection = detect(probe);
    println!(
        "bitty upgrade — install source: {} ({})",
        detection.channel.token(),
        detection.evidence
    );
    if rollback {
        return rollback_install(probe, &detection, dry_run);
    }
    if detection.channel == Channel::Unknown {
        return Err(Failure::new(EXIT_GENERIC, unknown_channel_message()));
    }
    // Delegated channels never need the release pointer: a package manager
    // owns the version, and a system-owned install must refuse before any
    // network access. `--check` is the read-only version comparison.
    if !detection.channel.is_self_managed() {
        return delegate(&detection, dry_run, yes);
    }
    let tag = fetch_latest_tag(probe, &base)?;
    let running = running_semver()?;
    let latest = Semver::parse_tag(&tag).ok_or_else(|| {
        Failure::new(
            EXIT_GENERIC,
            format!("published tag {tag:?} is not a stable vX.Y.Z release"),
        )
    })?;
    println!("  installed: {running}");
    println!("  latest:    {latest}");
    if latest <= running {
        println!("bitty {running} is current; nothing to do");
        return Ok(EXIT_OK);
    }
    self_update(probe, &detection, &base, &tag, running, latest, dry_run)
}

/// `--check`: read-only version comparison.
fn check_only(probe: &Probe<'_>, base: &str) -> Outcome {
    let tag = fetch_latest_tag(probe, base)?;
    let running = running_semver()?;
    let latest = Semver::parse_tag(&tag).ok_or_else(|| {
        Failure::new(
            EXIT_GENERIC,
            format!("published tag {tag:?} is not a stable vX.Y.Z release"),
        )
    })?;
    println!("installed: {running}");
    println!("latest:    {latest}");
    if latest > running {
        println!("update available: run `bitty upgrade`");
        Ok(EXIT_UPDATE_AVAILABLE)
    } else {
        println!("bitty {running} is current");
        Ok(EXIT_OK)
    }
}

/// Self-managed update: fetch, verify, replace atomically, report.
fn self_update(
    probe: &Probe<'_>,
    detection: &Detection,
    base: &str,
    tag: &str,
    running: Semver,
    latest: Semver,
    dry_run: bool,
) -> Outcome {
    let location = detection.location.as_ref().ok_or_else(|| {
        Failure::new(
            EXIT_GENERIC,
            format!(
                "{} install has no self-managed location",
                detection.channel.token()
            ),
        )
    })?;
    if cfg!(windows) {
        return Err(Failure::new(
            EXIT_PERM,
            "a running bitty.exe cannot be replaced in place; re-run the installer in a new terminal: irm https://cdn.bitty.run/install/install.ps1 | iex",
        ));
    }
    let asset = asset_name(detection.channel)?;
    let asset_url = format!("{base}/releases/{tag}/{asset}");
    let sums_url = format!("{base}/releases/{tag}/SHA256SUMS");
    let prev = prev_path(&location.binary)?;
    println!("  plan: download {asset_url}");
    println!(
        "        verify sha256 from SHA256SUMS, replace {} (backup {})",
        location.binary.display(),
        prev.display()
    );
    if dry_run {
        println!("dry-run: nothing was downloaded or written");
        return Ok(EXIT_OK);
    }

    let sums = fetch_text(
        probe,
        base,
        &format!("releases/{tag}/SHA256SUMS"),
        SHA256SUMS_MAX_BYTES,
    )?;
    let expected = digest_for(&sums, &asset).ok_or_else(|| {
        Failure::new(
            EXIT_GENERIC,
            format!("{sums_url} has no unambiguous entry for {asset}; refusing to install an unverified asset"),
        )
    })?;

    let staged = staged_path(&location.bin_dir);
    let staged_result = (|| -> Result<(), Failure> {
        let status = run_curl(&asset_url, &staged, ASSET_MAX_BYTES)?;
        require_http_ok(&asset_url, status)?;
        let actual = sha256_file(&staged)
            .map_err(|error| io_failure(&format!("cannot hash {}", staged.display()), &error))?;
        if !actual.eq_ignore_ascii_case(&expected) {
            return Err(Failure::new(
                EXIT_GENERIC,
                format!(
                    "checksum mismatch for {asset}: expected {expected}, got {actual}; the installed binary was not changed"
                ),
            ));
        }
        if let Ok(handle) = std::fs::File::open(&staged) {
            let _ = handle.sync_all();
        }
        make_executable(&staged).map_err(|error| {
            io_failure(
                &format!("cannot mark {} executable", staged.display()),
                &error,
            )
        })?;
        install_binary(&staged, &location.binary, &prev)
    })();
    if let Err(failure) = staged_result {
        let _ = std::fs::remove_file(&staged);
        return Err(failure);
    }

    if let Err(failure) = probe_version(&location.binary) {
        let restored = std::fs::rename(&prev, &location.binary);
        return Err(Failure::new(
            EXIT_GENERIC,
            match restored {
                Ok(()) => format!(
                    "the new binary failed its `--version` smoke ({}) and was rolled back",
                    failure.message
                ),
                Err(error) => format!(
                    "the new binary failed its `--version` smoke ({}) and the backup could not be restored: {error}",
                    failure.message
                ),
            },
        ));
    }

    refresh_marker(probe.env, location, &latest);
    println!("upgraded bitty {running} -> {latest}");
    println!("  binary: {}", location.binary.display());
    println!(
        "  backup: {} (restore with `bitty upgrade --rollback`)",
        prev.display()
    );
    Ok(EXIT_OK)
}

/// Delegated channels: print the exact command; execute user-scoped ones only
/// with `--yes`; never execute a system-owned one.
fn delegate(detection: &Detection, dry_run: bool, yes: bool) -> Outcome {
    let channel = detection.channel;
    let command = channel
        .delegated_command(detection.helper.as_deref())
        .ok_or_else(|| {
            Failure::new(
                EXIT_GENERIC,
                format!("{} has no upgrade command", channel.token()),
            )
        })?;
    println!("  command: {command}");
    if dry_run {
        // A plan never carries the permission class: nothing was attempted.
        println!("dry-run: nothing was executed");
        return Ok(EXIT_OK);
    }
    if channel.is_system_owned() {
        return Err(Failure::new(
            EXIT_PERM,
            format!("{command} is system-owned: bitty never elevates. Run it yourself."),
        ));
    }
    if !yes {
        println!("not executed: pass --yes to run `{command}`");
        return Ok(EXIT_OK);
    }
    let argv = channel
        .delegated_argv(detection.helper.as_deref())
        .ok_or_else(|| {
            Failure::new(
                EXIT_GENERIC,
                format!("{} has no executable upgrade command", channel.token()),
            )
        })?;
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .map_err(|error| {
            Failure::new(EXIT_GENERIC, format!("cannot run `{}`: {error}", argv[0]))
        })?;
    if status.success() {
        println!("{command} completed");
        Ok(EXIT_OK)
    } else {
        Err(Failure::new(
            EXIT_GENERIC,
            format!("`{command}` failed ({status})"),
        ))
    }
}

/// `--rollback`: restore the verified backup over the current binary.
fn rollback_install(probe: &Probe<'_>, detection: &Detection, dry_run: bool) -> Outcome {
    if !detection.channel.is_self_managed() {
        return Err(Failure::new(
            EXIT_GENERIC,
            format!(
                "rollback is only supported for script and AppImage installs (detected {}); use the channel's own downgrade path",
                detection.channel.token()
            ),
        ));
    }
    let location = detection
        .location
        .as_ref()
        .ok_or_else(|| Failure::new(EXIT_GENERIC, "install location is unknown"))?;
    if cfg!(windows) {
        return Err(Failure::new(
            EXIT_PERM,
            "a running bitty.exe cannot be replaced in place; re-run the installer with the older version instead",
        ));
    }
    let prev = prev_path(&location.binary)?;
    if !prev.is_file() {
        return Err(Failure::new(
            EXIT_GENERIC,
            format!("no backup at {}; nothing to roll back", prev.display()),
        ));
    }
    let backup = probe_version(&prev).map_err(|failure| {
        Failure::new(
            EXIT_GENERIC,
            format!(
                "{} does not run as a bitty binary ({}); refusing to restore it",
                prev.display(),
                failure.message
            ),
        )
    })?;
    println!("  backup: {} (reports bitty {backup})", prev.display());
    if dry_run {
        println!("dry-run: nothing was written");
        return Ok(EXIT_OK);
    }
    std::fs::rename(&prev, &location.binary).map_err(|error| {
        io_failure(
            &format!(
                "cannot restore {} over {}",
                prev.display(),
                location.binary.display()
            ),
            &error,
        )
    })?;
    refresh_marker(probe.env, location, &backup);
    println!("rolled back to bitty {backup}");
    println!("  binary: {}", location.binary.display());
    Ok(EXIT_OK)
}

/// Unknown-channel refusal: every manual path, plus the docs link.
fn unknown_channel_message() -> String {
    format!(
        "cannot detect how bitty was installed. Upgrade manually:\n\
         \x20 install script: curl -fsSL https://cdn.bitty.run/install/install.sh | bash\n\
         \x20 Homebrew:       brew upgrade bitty\n\
         \x20 Scoop:          scoop update bitty\n\
         \x20 package manager: use your distribution's upgrade command\n\
         docs: {UPGRADE_DOCS_URL}"
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Self-removing scratch directory.
    #[cfg(unix)]
    struct Scratch {
        path: PathBuf,
    }

    #[cfg(unix)]
    impl Scratch {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "bitty-upgrade-unit-{tag}-{}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create scratch dir");
            Self { path }
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    #[cfg(unix)]
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[cfg(unix)]
    fn write_stub(dir: &Path, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub");
    }

    #[cfg(unix)]
    fn detect_with(
        exe: PathBuf,
        path_dirs: Vec<PathBuf>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Detection {
        detect(&Probe {
            exe,
            path_dirs,
            env,
        })
    }

    #[test]
    fn semver_parses_and_orders_stable_versions() {
        assert_eq!(
            Semver::parse("0.0.27"),
            Some(Semver {
                major: 0,
                minor: 0,
                patch: 27
            })
        );
        assert_eq!(Semver::parse("v1.2.3"), Semver::parse("1.2.3"));
        assert!(Semver::parse("1.2.3-rc.1").is_none());
        assert!(Semver::parse("1.2.3+build").is_none());
        assert!(Semver::parse("1.2").is_none());
        assert!(Semver::parse("1.2.3.4").is_none());
        assert!(Semver::parse("1.2.x").is_none());
        assert!(Semver::parse("").is_none());
        assert!(Semver::parse_tag("0.0.27").is_none());
        assert!(Semver::parse_tag("v0.0.27").is_some());
        assert!(Semver::parse("0.0.9") < Semver::parse("0.0.10"));
    }

    #[test]
    fn sha256sums_requires_one_unambiguous_entry() {
        let sums = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  bitty-x86_64-unknown-linux-gnu
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  bitty-aarch64-unknown-linux-gnu
";
        assert_eq!(
            digest_for(sums, "bitty-x86_64-unknown-linux-gnu").as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            digest_for(sums, "bitty-aarch64-unknown-linux-gnu").as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert!(digest_for(sums, "bitty-x86_64-apple-darwin").is_none());
        assert!(digest_for(sums, "bitty-x86_64-unknown-linux-gn").is_none());
        let binary_mode = format!("{} *bitty-x86_64-unknown-linux-gnu\n", "c".repeat(64));
        assert!(digest_for(&binary_mode, "bitty-x86_64-unknown-linux-gnu").is_some());
        let duplicate = format!("{0}  asset\n{0}  asset\n", "d".repeat(64));
        assert!(digest_for(&duplicate, "asset").is_none());
        let short = "abc  asset\n";
        assert!(digest_for(short, "asset").is_none());
        assert!(digest_for("", "asset").is_none());
    }

    #[test]
    fn marker_round_trips_and_rejects_malformed_input() {
        let marker = Marker {
            channel: "script".to_string(),
            prefix: "/home/user/.local".to_string(),
            bin_dir: "/home/user/.local/bin".to_string(),
            version: "0.0.27".to_string(),
            binary_sha256: "a".repeat(64),
            installed_at: "2026-11-06T10:00:00Z".to_string(),
        };
        let json = marker.to_json();
        assert_eq!(Marker::parse(&json), Some(marker.clone()));
        // Escaped quote/backslash survive a round trip.
        let escaped = Marker {
            prefix: "/opt/we\"ird\\path".to_string(),
            ..marker.clone()
        };
        assert_eq!(Marker::parse(&escaped.to_json()), Some(escaped));
        assert!(Marker::parse("{}").is_none());
        assert!(Marker::parse("{\"channel\":\"script\"}").is_none());
        assert!(Marker::parse("not json").is_none());
        assert!(Marker::parse("{\"channel\":\"script\",}").is_none());
        assert!(Marker::parse(&format!("{json} trailing")).is_none());
    }

    #[test]
    fn iso8601_matches_known_epochs() {
        assert_eq!(iso8601_utc(UNIX_EPOCH), "1970-01-01T00:00:00Z".to_string());
        assert_eq!(
            iso8601_utc(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            "2023-11-14T22:13:20Z".to_string()
        );
        assert_eq!(
            iso8601_utc(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00Z".to_string()
        );
    }

    #[test]
    fn curl_argv_pins_https_and_bounds() {
        let argv = curl_argv(
            "https://cdn.bitty.run/releases/v0.0.27/bitty-x86_64-unknown-linux-gnu",
            Path::new("/tmp/staged"),
            ASSET_MAX_BYTES,
        )
        .expect("https argv");
        assert!(argv.iter().any(|arg| arg == "=https"));
        assert!(argv.iter().any(|arg| arg == "=https"));
        assert!(
            argv.windows(2)
                .any(|pair| pair == ["--proto-redir", "=https"])
        );
        assert!(
            argv.windows(2)
                .any(|pair| pair == ["--max-filesize", "268435456"])
        );
        assert!(
            argv.windows(2)
                .any(|pair| pair == ["--connect-timeout", "15"])
        );
        assert!(argv.windows(2).any(|pair| pair == ["--max-redirs", "3"]));
        assert!(!argv.iter().any(|arg| arg == "--insecure"));
        assert!(!argv.iter().any(|arg| arg == "--cacert"));
        assert!(curl_argv("http://example.com/x", Path::new("/tmp/s"), 1).is_err());
        let loopback = curl_argv("http://127.0.0.1:8080/x", Path::new("/tmp/s"), 1)
            .expect("loopback http argv");
        assert!(loopback.iter().any(|arg| arg == "=https,http"));
        assert!(
            loopback
                .windows(2)
                .any(|pair| pair == ["--proto-redir", "=https"])
        );
    }

    #[test]
    fn marker_path_follows_the_platform_data_dir() {
        let env = env_map(&[
            ("XDG_DATA_HOME", "/data"),
            ("HOME", "/home/user"),
            ("LOCALAPPDATA", "/local"),
        ]);
        let lookup = |key: &str| env.get(key).cloned();
        let path = marker_path(&lookup).expect("marker path");
        if cfg!(windows) {
            assert!(path.ends_with("bitty/install.json") || path.ends_with("bitty\\install.json"));
        } else {
            assert_eq!(path, PathBuf::from("/data/bitty/install.json"));
        }
        let relative = env_map(&[("XDG_DATA_HOME", "relative"), ("HOME", "/home/user")]);
        let lookup = |key: &str| relative.get(key).cloned();
        let path = marker_path(&lookup).expect("fallback marker path");
        if !cfg!(windows) {
            assert_eq!(
                path,
                PathBuf::from("/home/user/.local/share/bitty/install.json")
            );
        }
        let empty: HashMap<String, String> = HashMap::new();
        let lookup = |key: &str| empty.get(key).cloned();
        assert!(marker_path(&lookup).is_none());
    }

    #[test]
    fn version_output_parsing() {
        assert_eq!(
            version_from_output("bitty 0.0.27 (stable abc1234)"),
            Semver::parse("0.0.27")
        );
        assert!(version_from_output("0.0.27").is_none());
        assert!(version_from_output("bitty dev").is_none());
    }

    #[test]
    fn package_query_parsing() {
        assert_eq!(
            package_from_pacman_query("/usr/bin/bitty is owned by bitty 0.0.27-1"),
            Some("bitty".to_string())
        );
        assert_eq!(package_from_pacman_query("error: No package owns /x"), None);
    }

    #[test]
    fn path_prefix_is_component_wise() {
        assert!(path_starts_with(
            Path::new("/opt/homebrew/Cellar/bitty/0.0.27/bin/bitty"),
            Path::new("/opt/homebrew/Cellar/bitty")
        ));
        assert!(!path_starts_with(
            Path::new("/opt/homebrew/Cellar/bitty-extra/0.0.27/bin/bitty"),
            Path::new("/opt/homebrew/Cellar/bitty")
        ));
        assert!(path_starts_with(
            Path::new("/nix/store"),
            Path::new("/nix/store")
        ));
    }

    #[test]
    fn unknown_channel_names_manual_paths_and_docs() {
        let message = unknown_channel_message();
        assert!(message.contains("install.sh"));
        assert!(message.contains("brew upgrade bitty"));
        assert!(message.contains(UPGRADE_DOCS_URL));
    }

    /// The required detection table: one stub per channel, plus `unknown`.
    #[cfg(unix)]
    #[test]
    fn detection_table_covers_every_channel_and_unknown() {
        let scratch = Scratch::new("detect");
        let stubs = scratch.join("stubs");
        std::fs::create_dir_all(&stubs).expect("stub dir");

        // script: marker names the binary's directory.
        let script_bin = scratch.join("script/bin");
        std::fs::create_dir_all(&script_bin).expect("script bin");
        let script_exe = script_bin.join("bitty");
        std::fs::write(&script_exe, b"#!/bin/sh\nexit 0\n").expect("script exe");
        let marker = Marker {
            channel: "script".to_string(),
            prefix: scratch.join("script").display().to_string(),
            bin_dir: script_bin.display().to_string(),
            version: "0.0.27".to_string(),
            binary_sha256: "a".repeat(64),
            installed_at: "2026-11-06T10:00:00Z".to_string(),
        };
        let marker_file = scratch.join("data/bitty/install.json");
        marker.write(&marker_file).expect("write marker");
        let script_env = env_map(&[
            ("XDG_DATA_HOME", &scratch.join("data").display().to_string()),
            ("HOME", &scratch.path.display().to_string()),
        ]);
        let lookup = |key: &str| script_env.get(key).cloned();
        let detection = detect_with(script_exe.clone(), vec![stubs.clone()], &lookup);
        assert_eq!(detection.channel, Channel::Script);
        assert_eq!(
            detection
                .location
                .as_ref()
                .map(|location| location.bin_dir.clone()),
            Some(script_bin.clone())
        );

        // appimage: $APPIMAGE is the running file.
        let appimage = scratch.join("apps/Bitty.AppImage");
        std::fs::create_dir_all(appimage.parent().expect("appimage dir")).expect("appimage dir");
        std::fs::write(&appimage, b"#!/bin/sh\nexit 0\n").expect("appimage file");
        let appimage_env = env_map(&[("APPIMAGE", &appimage.display().to_string())]);
        let lookup = |key: &str| appimage_env.get(key).cloned();
        assert_eq!(
            detect_with(appimage.clone(), vec![stubs.clone()], &lookup).channel,
            Channel::AppImage
        );

        // homebrew: cellar layout plus `brew` on PATH.
        write_stub(&stubs, "brew", "#!/bin/sh\nexit 0\n");
        let cellar = scratch.join("cellar");
        let brew_exe = cellar.join("bitty/0.0.27/bin/bitty");
        std::fs::create_dir_all(brew_exe.parent().expect("brew bin")).expect("brew bin");
        std::fs::write(&brew_exe, b"x").expect("brew exe");
        let brew_env = env_map(&[
            ("HOMEBREW_CELLAR", &cellar.display().to_string()),
            ("HOME", &scratch.path.display().to_string()),
        ]);
        let lookup = |key: &str| brew_env.get(key).cloned();
        assert_eq!(
            detect_with(brew_exe.clone(), vec![stubs.clone()], &lookup).channel,
            Channel::Homebrew
        );

        // scoop: app layout plus `scoop` on PATH.
        write_stub(&stubs, "scoop", "#!/bin/sh\nexit 0\n");
        let scoop_root = scratch.join("scoop");
        let scoop_exe = scoop_root.join("apps/bitty/current/bitty.exe");
        std::fs::create_dir_all(scoop_exe.parent().expect("scoop bin")).expect("scoop bin");
        std::fs::write(&scoop_exe, b"x").expect("scoop exe");
        let scoop_env = env_map(&[("SCOOP", &scoop_root.display().to_string())]);
        let lookup = |key: &str| scoop_env.get(key).cloned();
        assert_eq!(
            detect_with(scoop_exe.clone(), vec![stubs.clone()], &lookup).channel,
            Channel::Scoop
        );

        // nix: store path (no tool needed).
        let store = scratch.join("nix/store");
        let nix_exe = store.join("abc-bitty-0.0.27/bin/bitty");
        std::fs::create_dir_all(nix_exe.parent().expect("nix bin")).expect("nix bin");
        std::fs::write(&nix_exe, b"x").expect("nix exe");
        let nix_env = env_map(&[("NIX_STORE_DIR", &store.display().to_string())]);
        let lookup = |key: &str| nix_env.get(key).cloned();
        assert_eq!(
            detect_with(nix_exe.clone(), vec![stubs.clone()], &lookup).channel,
            Channel::Nix
        );

        // apt: dpkg-query owns the binary.
        let apt_bin = scratch.join("apt/bin");
        std::fs::create_dir_all(&apt_bin).expect("apt bin");
        let apt_exe = apt_bin.join("bitty");
        std::fs::write(&apt_exe, b"x").expect("apt exe");
        let dpkg_stubs = scratch.join("dpkg-stubs");
        std::fs::create_dir_all(&dpkg_stubs).expect("dpkg stubs");
        write_stub(&dpkg_stubs, "dpkg-query", "#!/bin/sh\nexit 0\n");
        let empty_env: HashMap<String, String> = HashMap::new();
        let lookup = |key: &str| empty_env.get(key).cloned();
        assert_eq!(
            detect_with(apt_exe.clone(), vec![dpkg_stubs.clone()], &lookup).channel,
            Channel::Apt
        );

        // dnf / rpm: rpm owns the binary; `dnf` decides the verb.
        let rpm_stubs = scratch.join("rpm-stubs");
        std::fs::create_dir_all(&rpm_stubs).expect("rpm stubs");
        write_stub(&rpm_stubs, "rpm", "#!/bin/sh\nexit 0\n");
        assert_eq!(
            detect_with(apt_exe.clone(), vec![rpm_stubs.clone()], &lookup).channel,
            Channel::Rpm
        );
        write_stub(&rpm_stubs, "dnf", "#!/bin/sh\nexit 0\n");
        assert_eq!(
            detect_with(apt_exe.clone(), vec![rpm_stubs.clone()], &lookup).channel,
            Channel::Dnf
        );

        // pacman: repository package.
        let pacman_stubs = scratch.join("pacman-stubs");
        std::fs::create_dir_all(&pacman_stubs).expect("pacman stubs");
        write_stub(
            &pacman_stubs,
            "pacman",
            "#!/bin/sh\ncase \"$*\" in\n  *-Qo*) echo \"/usr/bin/bitty is owned by bitty 0.0.27-1\"; exit 0;;\n  *-Si*) exit 0;;\nesac\nexit 1\n",
        );
        let detection = detect_with(apt_exe.clone(), vec![pacman_stubs.clone()], &lookup);
        assert_eq!(detection.channel, Channel::Pacman);

        // aur: owned by pacman, absent from the configured repositories.
        write_stub(
            &pacman_stubs,
            "pacman",
            "#!/bin/sh\ncase \"$*\" in\n  *-Qo*) echo \"/usr/bin/bitty is owned by bitty 0.0.27-1\"; exit 0;;\n  *-Si*) exit 1;;\nesac\nexit 1\n",
        );
        write_stub(&pacman_stubs, "yay", "#!/bin/sh\nexit 0\n");
        let detection = detect_with(apt_exe.clone(), vec![pacman_stubs.clone()], &lookup);
        assert_eq!(detection.channel, Channel::Aur);
        assert_eq!(detection.helper.as_deref(), Some("yay"));

        // unknown: nothing matches.
        let unknown = scratch.join("unknown");
        std::fs::create_dir_all(&unknown).expect("unknown dir");
        assert_eq!(
            detect_with(unknown.join("bitty"), vec![unknown.clone()], &lookup).channel,
            Channel::Unknown
        );
    }

    /// A marker that names another directory never claims this binary.
    #[cfg(unix)]
    #[test]
    fn stale_marker_falls_through_to_unknown() {
        let scratch = Scratch::new("stale");
        let marker = Marker {
            channel: "script".to_string(),
            prefix: scratch.join("elsewhere").display().to_string(),
            bin_dir: scratch.join("elsewhere/bin").display().to_string(),
            version: "0.0.27".to_string(),
            binary_sha256: "a".repeat(64),
            installed_at: "2026-11-06T10:00:00Z".to_string(),
        };
        let marker_file = scratch.join("data/bitty/install.json");
        marker.write(&marker_file).expect("write marker");
        let env = env_map(&[("XDG_DATA_HOME", &scratch.join("data").display().to_string())]);
        let lookup = |key: &str| env.get(key).cloned();
        let exe = scratch.join("bin/bitty");
        std::fs::create_dir_all(exe.parent().expect("bin")).expect("bin");
        std::fs::write(&exe, b"x").expect("exe");
        assert_eq!(
            detect_with(exe, vec![scratch.join("stubs")], &lookup).channel,
            Channel::Unknown
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_binary_keeps_the_previous_bytes_and_rejects_foreign_staging() {
        let scratch = Scratch::new("install");
        let bin_dir = scratch.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let binary = bin_dir.join("bitty");
        std::fs::write(&binary, b"old").expect("old binary");
        let prev = prev_path(&binary).expect("prev path");
        let staged = bin_dir.join(".staged");
        std::fs::write(&staged, b"new").expect("staged");
        install_binary(&staged, &binary, &prev).expect("install");
        assert_eq!(std::fs::read(&binary).expect("binary"), b"new");
        assert_eq!(std::fs::read(&prev).expect("prev"), b"old");
        assert!(!staged.exists());

        let foreign = scratch.join("elsewhere/.staged");
        std::fs::create_dir_all(foreign.parent().expect("foreign dir")).expect("foreign dir");
        std::fs::write(&foreign, b"new").expect("foreign staged");
        assert!(install_binary(&foreign, &binary, &prev).is_err());
        assert_eq!(std::fs::read(&binary).expect("binary"), b"new");
    }
}
