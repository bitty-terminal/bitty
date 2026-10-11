//! WSL2 trampoline for component binaries (issue #1908, part of #1902 delta 7).
//!
//! Windows component binaries that must run inside a WSL2 guest do so through
//! an explicit trampoline: the Windows host spawns `wsl.exe` (PATH-resolved,
//! fixed argv, no shell) which `exec`s the Linux helper inside the guest.
//! Unix hosts never use this module (all entry points are no-ops there, so
//! Unix behavior is 100% unchanged).
//!
//! # Guest distro selection and persistence
//!
//! - Explicit `--distro <name>` wins (CLI flag, `bitty component --help`).
//! - Else `BITTY_WSL_DISTRO` environment/config wins (`select_wsl_distro` takes
//!   both as pure inputs; the caller reads the environment).
//! - Else the trampoline is disabled (native execution, `None`). There is no
//!   silent default: to run with the `wsl.exe` default distro pass the literal
//!   `default` (`--distro default` or `BITTY_WSL_DISTRO=default`), which maps
//!   to [`WslDistro::Default`] (no `--distribution` flag, `wsl.exe` picks its
//!   configured default).
//! - The literal `default` is reserved and never a distro name
//!   ([`valid_wsl_distro`] rejects it); any other invalid name fails closed as
//!   usage (exit `2`), never a silent pick.
//! - When a distro is selected but WSL is absent (no `wsl.exe` or both probes
//!   fail), execution fails closed with an actionable diagnostic (exit `1`):
//!   install or enable WSL2 (Store version for `--version`; inbox builds use
//!   `--status` as presence-only fallback, see below), or run natively without
//!   `--distro`. There is no silent fallback to native when WSL was requested.
//! - `add`/`install` are staging/fetch operations only (they never execute the
//!   component). When a distro is selected they persist it per-component to
//!   `<user>/<name>/wsl_distro` (`WSL_DISTRO_FILE`, `default` or the distro
//!   name plus a newline, atomic write like the TOFU pin). Omission preserves
//!   any previous placement (never deletes); later component runs (broker)
//!   read that file when no explicit in-memory selection overrides it. Unix
//!   records the file but never acts on it (native execution, no-op).
//!
//! # Windows to guest path mapping (primary direction)
//!
//! Component-store paths are Windows spellings on the host (for example
//! `C:\Users\you\AppData\Local\bitty\components\net\0.0.1\bitty-net.exe`)
//! and must be mapped to guest spellings before `exec` (for example
//! `/mnt/c/Users/you/AppData/Local/bitty/components/net/0.0.1/bitty-net.exe`).
//! [`windows_path_to_guest`] implements exactly:
//!
//! - Drive-absolute `X:\...` or `X:/...` (`X` is `A-Z`/`a-z`) maps to
//!   `/mnt/<lower-x>/...` with backslashes rewritten to slashes. `..`
//!   segments are preserved verbatim (no silent canonicalization); the guest
//!   resolves them.
//! - UNC `\\wsl$\<distro>\...` and `\\wsl.localhost\<distro>\...` (exact,
//!   case-sensitive prefix) map to guest-absolute `/...` (backslashes to
//!   slashes, `\\wsl$\Ubuntu\home\you` becomes `/home/you`). An empty
//!   remainder maps to `/` (guest root).
//! - Everything else fails closed as [`WslError::UnmappablePath`] (exit `2`):
//!   device paths (`\\?\`, `\\.\`), other UNC (`\\server\share`), drive-
//!   relative (`C:foo`), root-relative (`\foo`), relative (`foo\bar`), guest
//!   absolute (`/home/you`) passed as a Windows path, empty, overlong, or
//!   NUL/control-bearing inputs. No silent mangling.
//!
//! The reverse ([`guest_mnt_to_windows`]) exists for diagnostics only and maps
//! exactly `/mnt/<lower-letter>/...` back to `X:\...`; every other guest path
//! (notably `/home/...`, `/etc/...`) fails closed because it has no Windows
//! spelling without distro context.
//!
//! # Escaping rules (no shell on either side)
//!
//! `wsl.exe` argv is built by [`wsl_argv`] as discrete elements passed to
//! [`std::process::Command`] (never `sh -c`, never `cmd /c`). Metacharacters
//! in every argument are inert data; spaces survive as a single argv element;
//! backslash rewriting happens only inside [`windows_path_to_guest`], never in
//! general argv. Fixed argv is preserved through the trampoline.
//!
//! # Stdio and signal passthrough semantics
//!
//! - Stdin, stdout, and stderr are OS pipes passed through `wsl.exe`
//!   unchanged (binary-safe, no CRLF translation by bitty; the guest helper
//!   speaks the native-component wire protocol over stdio exactly as on Unix).
//! - Ctrl-C (console control on Windows, `SIGINT` in the guest) propagates via
//!   `wsl.exe` to the guest helper; bitty does not synthesize or swallow it.
//!   Closing stdin (idle stop, shutdown) delivers EOF to the guest helper,
//!   which exits; the broker then reaps exactly as for a native child.
//! - Exit-code fidelity is 1:1: the `wsl.exe` exit status is the helper exit
//!   status, and the spawner preserves it with no remapping (a helper exit of
//!   `N` surfaces as `N`; a trampoline failure surfaces as a spawn error, not
//!   as the helper code).
//!
//! # Detection (Windows-gated, bounded, fail-closed)
//!
//! [`detect_wsl_context`] on Windows probes presence with a bounded,
//! honestly-capped read (see below) and a bounded wait ([`WSL_PROBE_TIMEOUT`]):
//! first `wsl.exe --version` (Store builds; returns a version string), else
//! `wsl.exe --status` as presence-only fallback for older inbox builds that
//! lack `--version` (returns `present-but-unversioned`, no version floor is
//! enforced, so proceeding is safe). Absent `wsl.exe` or both probes failing
//! returns [`WslError::AbsentWsl`] with the actionable diagnostic naming the
//! Store upgrade (exit `1`). Choosing fallback-over-fail-closed for
//! `--version`-only absence is deliberate: inbox WSL2 is operational, just
//! unversioned; genuinely absent WSL still fails closed. Probe output is
//! decoded as UTF-8 lossy, with UTF-16LE (BOM or NUL-interleaved) handled so
//! redirected inbox output never surfaces as NULs. On Unix this function is a
//! no-op returning `Ok(None)` with zero spawn.
//!
//! Bound honesty: the probe uses `spawn` with piped stdout (stderr discarded
//! to null so the child never blocks on it), reads at most
//! [`WSL_VERSION_OUTPUT_MAX_BYTES`] + 1 bytes while reading (excess fails as
//! [`WslError::ProbeFailed`] without buffering the remainder), and kills plus
//! reaps the child on timeout, so no probe outlives failure.
//!
//! # Guest environment (WSLENV, clean by design)
//!
//! The guest receives ONLY variables named in `WSLENV` (WSL interop allowlist).
//! The broker sets `WSLENV` explicitly for trampoline spawns: it preserves the
//! caller's existing entries (from the hermetic [`ComponentEnv`], which may
//! carry a user `WSLENV`), appends the forwarded allowlisted names (proxy,
//! TLS, locale), and NEVER includes secret-shaped names (`*_TOKEN`, `*_KEY`,
//! `*_SECRET`, filtered from both existing and forwarding lists, even if the
//! caller listed them). Forwarded names are bare (no `/p` conversion; proxy
//! URLs need none); existing suffixes (`/p`, `/u`, ...) are preserved
//! verbatim. The guest environment is otherwise clean by design: nothing
//! outside `WSLENV` crosses, even though the Windows `wsl.exe` process itself
//! carries the cleared-plus-allowlist environment.
//!
//! # UNC-to-launch distro binding
//!
//! [`windows_path_to_guest`] maps `\\wsl$\<distroA>\...` to `/...` (dropping
//! the source namespace). Executing that `/...` under a different launch
//! distro `B` would address B's bytes, not the verified A's bytes
//! (substitution across the namespace boundary). [`check_unc_distro_binding`]
//! fails closed on any such mismatch: UNC paths require an explicit
//! [`WslDistro::Named`] launch equal (case-sensitive) to the UNC distro;
//! [`WslDistro::Default`] with a UNC path fails (the default may differ).
//! Drive-absolute paths (`X:\...`, visible in every distro via `/mnt`) bind
//! to any launch distro.

use std::path::Path;
use std::time::Duration;

/// Trampoline executable (PATH-resolved, `wsl.exe` via `PATHEXT` on Windows,
// like `curl`/`tar` prior art in `component_seed.rs`).
pub const WSL_EXE: &str = "wsl.exe";

/// Environment/config key for guest distro selection.
pub const WSL_DISTRO_ENV: &str = "BITTY_WSL_DISTRO";

/// Maximum bytes of a distro name (`--distro` value).
pub const WSL_DISTRO_MAX_BYTES: usize = 64;

/// Maximum bytes of one path spelling converted by this module.
pub const WSL_PATH_MAX_BYTES: usize = 4096;

/// Maximum bytes of probe stdout kept while reading (`--version`/`--status`).
///
/// The probe reads at most this plus one byte; excess fails as `ProbeFailed`
/// without buffering the remainder (honestly bounded, unlike `output()`).
pub const WSL_VERSION_OUTPUT_MAX_BYTES: usize = 8192;

/// Bounded wait for one probe command (`--version` or `--status`).
pub const WSL_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-component persisted distro file name inside `<user>/<name>/`.
pub const WSL_DISTRO_FILE: &str = "wsl_distro";

/// Maximum bytes read from a persisted distro file (name plus newline).
pub const WSL_DISTRO_FILE_MAX_BYTES: u64 = 128;

/// Guest distro selection when the trampoline is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WslDistro {
    /// Use the `wsl.exe` configured default (no `--distribution` flag).
    Default,
    /// Use the named distro (`--distribution <name>`).
    Named(String),
}

/// Why WSL trampoline setup failed (all variants carry owned context).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WslError {
    /// Distro name violates the grammar (CLI input, exit 2).
    InvalidDistro(String),
    /// Path has no guest (or Windows) spelling (CLI input, exit 2).
    UnmappablePath(String),
    /// Helper guest path or argv element is invalid (internal, exit 2).
    InvalidHelper(String),
    /// `wsl.exe` absent or version probe failed (environmental, exit 1).
    AbsentWsl(String),
    /// Probe spawn failed for an unexpected I/O reason (exit 1).
    ProbeFailed(String),
}

impl WslError {
    /// Stable exit code: `2` usage, `1` environmental.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::InvalidDistro(_) | Self::UnmappablePath(_) | Self::InvalidHelper(_) => 2,
            Self::AbsentWsl(_) | Self::ProbeFailed(_) => 1,
        }
    }

    /// User-facing message (already complete).
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::InvalidDistro(detail)
            | Self::UnmappablePath(detail)
            | Self::InvalidHelper(detail)
            | Self::AbsentWsl(detail)
            | Self::ProbeFailed(detail) => detail.clone(),
        }
    }
}

impl std::fmt::Display for WslError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for WslError {}

/// Whether `name` is a valid WSL distro name.
///
/// Grammar: 1..=`WSL_DISTRO_MAX_BYTES` bytes, ASCII alphanumeric plus
/// `.`, `_`, `-`, never starting with `-` or `.`. The literal `default` is
/// reserved (it selects [`WslDistro::Default`], never a named distro) and
/// returns `false` here. Slashes, backslashes, whitespace, NUL, controls, and
/// shell metacharacters all return `false`.
#[must_use]
pub fn valid_wsl_distro(name: &str) -> bool {
    if name.is_empty() || name.len() > WSL_DISTRO_MAX_BYTES {
        return false;
    }
    if name == "default" {
        return false;
    }
    if name.contains('\0') {
        return false;
    }
    let mut chars = name.bytes();
    let first = chars.next().unwrap_or(b'-');
    if first == b'-' || first == b'.' {
        return false;
    }
    name.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Select the guest distro from explicit (`--distro`) and env/config inputs.
///
/// Pure (no I/O, no spawn): `explicit` is the `--distro` value, `env` is the
/// `BITTY_WSL_DISTRO` value. `None` means absent. Returns `None` for native
/// execution (trampoline disabled, no selection at all). The literal
/// `default` (either source) selects [`WslDistro::Default`]. Invalid names
/// fail closed as [`WslError::InvalidDistro`]. Explicit wins over env.
pub fn select_wsl_distro(
    explicit: Option<&str>,
    env: Option<&str>,
) -> Result<Option<WslDistro>, WslError> {
    if let Some(name) = explicit {
        if name == "default" {
            return Ok(Some(WslDistro::Default));
        }
        if !valid_wsl_distro(name) {
            return Err(WslError::InvalidDistro(format!(
                "invalid --distro {name:?} (want 1..={WSL_DISTRO_MAX_BYTES} bytes of [A-Za-z0-9._-], not starting with - or ., or the literal `default` for the wsl.exe default distro)"
            )));
        }
        return Ok(Some(WslDistro::Named(name.to_owned())));
    }
    if let Some(name) = env {
        if name == "default" {
            return Ok(Some(WslDistro::Default));
        }
        if !valid_wsl_distro(name) {
            return Err(WslError::InvalidDistro(format!(
                "invalid {WSL_DISTRO_ENV} {name:?} (want 1..={WSL_DISTRO_MAX_BYTES} bytes of [A-Za-z0-9._-], not starting with - or ., or the literal `default` for the wsl.exe default distro)"
            )));
        }
        return Ok(Some(WslDistro::Named(name.to_owned())));
    }
    Ok(None)
}

/// Map a Windows component-store path to its guest spelling.
///
/// See the module docs for the exact rules. Unmappable inputs fail closed as
/// [`WslError::UnmappablePath`]; there is no silent mangling and no
/// normalization beyond backslash-to-slash inside the mapped remainder (`..`
/// is preserved for the guest to resolve).
pub fn windows_path_to_guest(windows: &str) -> Result<String, WslError> {
    if windows.is_empty()
        || windows.len() > WSL_PATH_MAX_BYTES
        || windows.contains('\0')
        || windows.chars().any(char::is_control)
    {
        return Err(WslError::UnmappablePath(format!(
            "unmappable Windows path {windows:?} (want 1..={WSL_PATH_MAX_BYTES} bytes, no NUL or control characters)"
        )));
    }
    // UNC guest spellings: `\\wsl$\<distro>\...` and
    // `\\wsl.localhost\<distro>\...` (exact, case-sensitive prefix).
    for prefix in ["\\\\wsl$\\", "\\\\wsl.localhost\\"] {
        if let Some(rest) = windows.strip_prefix(prefix) {
            let (distro, remainder) = match rest.split_once(['\\', '/']) {
                Some((distro, remainder)) => (distro, remainder),
                None => (rest, ""),
            };
            // A bare `\\wsl$\<distro>` (no trailing separator) names the
            // distro root, which is the guest root `/`.
            let bare_root = !rest.contains(['\\', '/']);
            if bare_root {
                if !valid_wsl_distro(distro) && distro != "default" {
                    return Err(WslError::UnmappablePath(format!(
                        "unmappable Windows path {windows:?} (WSL UNC distro {distro:?} is not a valid distro name)"
                    )));
                }
                return Ok("/".to_string());
            }
            if distro.is_empty() || (!valid_wsl_distro(distro) && distro != "default") {
                return Err(WslError::UnmappablePath(format!(
                    "unmappable Windows path {windows:?} (WSL UNC distro {distro:?} is not a valid distro name)"
                )));
            }
            let guest = format!("/{}", remainder.replace('\\', "/"));
            if guest.len() > WSL_PATH_MAX_BYTES {
                return Err(WslError::UnmappablePath(format!(
                    "unmappable Windows path {windows:?} (mapped guest spelling exceeds {WSL_PATH_MAX_BYTES} bytes)"
                )));
            }
            return Ok(guest);
        }
    }
    // Drive-absolute `X:\...` or `X:/...`.
    let bytes = windows.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        let drive = bytes[0].to_ascii_lowercase() as char;
        let remainder = windows[3..].replace('\\', "/");
        let guest = if remainder.is_empty() {
            format!("/mnt/{drive}")
        } else {
            format!("/mnt/{drive}/{remainder}")
        };
        if guest.len() > WSL_PATH_MAX_BYTES {
            return Err(WslError::UnmappablePath(format!(
                "unmappable Windows path {windows:?} (mapped guest spelling exceeds {WSL_PATH_MAX_BYTES} bytes)"
            )));
        }
        return Ok(guest);
    }
    Err(WslError::UnmappablePath(format!(
        "unmappable Windows path {windows:?} (want drive-absolute X:\\... or WSL UNC \\\\wsl$\\<distro>\\...; device paths, drive-relative, relative, and guest-absolute spellings have no mapping)"
    )))
}

/// Map a guest `/mnt/<letter>/...` path back to its Windows spelling.
///
/// Diagnostics only. Exactly `/mnt/<lower-letter>` optionally followed by
/// `/...` maps to `X:\...` (uppercase drive, slashes to backslashes).
/// Everything else (notably `/home/...`) fails closed as
/// [`WslError::UnmappablePath`] because it has no Windows spelling without
/// distro context.
pub fn guest_mnt_to_windows(guest: &str) -> Result<String, WslError> {
    if guest.is_empty()
        || guest.len() > WSL_PATH_MAX_BYTES
        || guest.contains('\0')
        || guest.chars().any(char::is_control)
    {
        return Err(WslError::UnmappablePath(format!(
            "unmappable guest path {guest:?} (want 1..={WSL_PATH_MAX_BYTES} bytes, no NUL or control characters)"
        )));
    }
    let Some(rest) = guest.strip_prefix("/mnt/") else {
        return Err(WslError::UnmappablePath(format!(
            "unmappable guest path {guest:?} (only /mnt/<drive>/... has a Windows spelling; use the Windows-to-guest direction for spawn)"
        )));
    };
    let mut parts = rest.splitn(2, '/');
    let drive = parts.next().unwrap_or("");
    let remainder = parts.next().unwrap_or("");
    if drive.len() != 1 || !drive.bytes().all(|b| b.is_ascii_lowercase()) {
        return Err(WslError::UnmappablePath(format!(
            "unmappable guest path {guest:?} (want /mnt/<lowercase-drive-letter>/...)"
        )));
    }
    let drive_upper = drive.to_ascii_uppercase();
    let windows = if remainder.is_empty() {
        format!("{drive_upper}:\\")
    } else {
        format!("{drive_upper}:\\{}", remainder.replace('/', "\\"))
    };
    if windows.len() > WSL_PATH_MAX_BYTES {
        return Err(WslError::UnmappablePath(format!(
            "unmappable guest path {guest:?} (mapped Windows spelling exceeds {WSL_PATH_MAX_BYTES} bytes)"
        )));
    }
    Ok(windows)
}

/// Validate one guest helper path for `exec`.
///
/// Must be guest-absolute (`/...`), bounded, with no NUL/controls and no `..`
/// segment (component-store helpers never contain `..`; rejecting it here
/// keeps a mapped `..` from escaping the store via the trampoline).
fn valid_helper_guest_path(path: &str) -> bool {
    if path.is_empty() || path.len() > WSL_PATH_MAX_BYTES {
        return false;
    }
    if !path.starts_with('/') {
        return false;
    }
    if path.contains('\0') || path.chars().any(char::is_control) {
        return false;
    }
    // Reject `..` as a full segment (component-store helpers never contain
    // `..`; rejecting it keeps a mapped `..` from escaping via the trampoline).
    !path.split('/').any(|segment| segment == "..")
}

/// Build the fixed `wsl.exe` argv for one helper execution.
///
/// `distro` selects `--distribution` (`None` for [`WslDistro::Default`]
/// omits the flag and uses the `wsl.exe` default). `helper_guest_path` is the
/// already-mapped guest spelling (see [`windows_path_to_guest`]); `helper_argv`
/// is the fixed helper argv preserved verbatim (no shell, discrete elements).
/// Every element is bounded and NUL-free; violations fail closed as
/// [`WslError::InvalidHelper`].
pub fn wsl_argv(
    distro: &WslDistro,
    helper_guest_path: &str,
    helper_argv: &[String],
) -> Result<Vec<String>, WslError> {
    wsl_argv_with_cwd(distro, None, helper_guest_path, helper_argv)
}

/// Build the fixed `wsl.exe` argv with an optional guest working directory.
///
/// Like [`wsl_argv`], plus `guest_cwd` (already mapped, guest-absolute) is
/// passed as `--cd <dir>` before `--exec` so the helper starts in the
/// component version directory. `None` omits `--cd` (inherits the guest
/// default); `Some` must pass [`valid_helper_guest_path`]-style validation
/// (absolute, bounded, no `..`).
pub fn wsl_argv_with_cwd(
    distro: &WslDistro,
    guest_cwd: Option<&str>,
    helper_guest_path: &str,
    helper_argv: &[String],
) -> Result<Vec<String>, WslError> {
    if !valid_helper_guest_path(helper_guest_path) {
        return Err(WslError::InvalidHelper(format!(
            "invalid helper guest path {helper_guest_path:?} (want guest-absolute /... with no `..` segment)"
        )));
    }
    if let Some(cwd) = guest_cwd {
        if !valid_helper_guest_path(cwd) {
            return Err(WslError::InvalidHelper(format!(
                "invalid guest working directory {cwd:?} (want guest-absolute /... with no `..` segment)"
            )));
        }
    }
    for arg in helper_argv {
        if arg.len() > WSL_PATH_MAX_BYTES || arg.contains('\0') {
            return Err(WslError::InvalidHelper(
                "invalid helper argv element (want at most 4096 bytes, no NUL)".to_string(),
            ));
        }
        // The empty string is a valid (if unusual) argv element; any other
        // control-bearing element fails closed.
        if !arg.is_empty() && arg.chars().any(char::is_control) {
            return Err(WslError::InvalidHelper(
                "invalid helper argv element (control characters are not allowed)".to_string(),
            ));
        }
    }
    let mut argv = vec![WSL_EXE.to_string()];
    if let WslDistro::Named(name) = distro {
        argv.push("--distribution".to_string());
        argv.push(name.clone());
    }
    if let Some(cwd) = guest_cwd {
        argv.push("--cd".to_string());
        argv.push(cwd.to_owned());
    }
    argv.push("--exec".to_string());
    argv.push(helper_guest_path.to_owned());
    argv.extend(helper_argv.iter().cloned());
    Ok(argv)
}

/// Decode probe stdout that may be UTF-8 or UTF-16LE.
///
/// Inbox `wsl.exe` commonly writes UTF-16LE without a BOM when redirected to
/// a pipe; Store builds write UTF-8. This decodes UTF-16LE (BOM `FF FE` or
/// NUL-interleaved ASCII) to lossy UTF-8, otherwise falls back to UTF-8 lossy.
/// Pure (no I/O), testable on every platform.
#[must_use]
pub fn decode_probe_output(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return decode_utf16le(&bytes[2..]);
    }
    // Heuristic for BOM-less UTF-16LE ASCII (`V\0 e\0 r\0 ...`): even length,
    // every odd byte NUL, at least one NUL present.
    if bytes.len() >= 2
        && bytes.len() % 2 == 0
        && bytes.iter().skip(1).step_by(2).all(|&b| b == 0)
        && bytes.iter().step_by(2).any(|&b| b != 0)
    {
        return decode_utf16le(bytes);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// Lossy UTF-16LE decode (no new deps, ASCII-first, unpaired surrogates fail
/// closed to the replacement character, never panic).
fn decode_utf16le(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut chunks = bytes.chunks_exact(2);
    for pair in &mut chunks {
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        out.push(char::from_u32(u32::from(unit)).unwrap_or('\u{FFFD}'));
    }
    // A trailing odd byte cannot form a unit; surface it as a replacement.
    if !chunks.remainder().is_empty() {
        out.push('\u{FFFD}');
    }
    out
}

/// Outcome of one probe command (pure decision core, testable everywhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(any(windows, test))]
enum ProbeDecision {
    /// `wsl.exe` itself is missing (spawn `NotFound`): fail closed at once,
    /// no fallback can help.
    Missing,
    /// Command ran and succeeded: present (caller decodes stdout as needed).
    Present,
    /// Command ran but failed (non-zero exit, e.g. inbox `--version`
    /// unsupported): try the next fallback command, if any.
    Fallback,
    /// Spawn failed for a non-`NotFound` I/O reason: probe failure.
    SpawnFailed,
}

#[cfg(any(windows, test))]
impl ProbeDecision {
    /// Classify a spawn/result pair without touching the filesystem.
    fn classify(spawn_kind: Option<std::io::ErrorKind>, exit_success: bool) -> Self {
        match spawn_kind {
            Some(std::io::ErrorKind::NotFound) => Self::Missing,
            Some(_) => Self::SpawnFailed,
            None if exit_success => Self::Present,
            None => Self::Fallback,
        }
    }

    /// Map a spawn error through the classifier to a typed probe error.
    #[cfg(windows)]
    fn spawn_error(arg: &str, error: std::io::Error) -> WslError {
        match Self::classify(Some(error.kind()), false) {
            Self::Missing => WslError::AbsentWsl(absent_diagnostic()),
            _ => WslError::ProbeFailed(format!(
                "bitty component: cannot run `{WSL_EXE} {arg}`: {error}"
            )),
        }
    }
}

/// Run one probe command with an honestly bounded stdout read.
///
/// `spawn` with piped stdout (stderr discarded to null so the child never
/// blocks on it); the reader takes at most `WSL_VERSION_OUTPUT_MAX_BYTES + 1`
/// bytes while reading (excess fails as `ProbeFailed` without buffering the
/// remainder). On `WSL_PROBE_TIMEOUT` the child is killed and reaped before
/// returning, so no probe outlives failure. Returns `(success, stdout)`.
///
/// Windows-only in effect (the caller is `#[cfg(windows)]`); defined on every
/// platform so unit tests exercise the bounded-read core via `read` injection
/// without spawning.
#[cfg(windows)]
fn run_probe_command(arg: &str) -> Result<(bool, Vec<u8>), WslError> {
    use std::io::Read as _;
    use std::process::Stdio;

    let mut child = std::process::Command::new(WSL_EXE)
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| ProbeDecision::spawn_error(arg, error))?;
    let stdout = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<Vec<u8>>>();
    std::thread::Builder::new()
        .name("bitty-wsl-probe-read".into())
        .spawn(move || {
            let result = (|| -> std::io::Result<Vec<u8>> {
                let Some(pipe) = stdout else {
                    return Ok(Vec::new());
                };
                // One byte past the cap proves excess without keeping it.
                let mut bounded = pipe.take(WSL_VERSION_OUTPUT_MAX_BYTES as u64 + 1);
                let mut buf = Vec::new();
                bounded.read_to_end(&mut buf)?;
                Ok(buf)
            })();
            let _ = tx.send(result);
        })
        .map_err(|error| {
            let _ = child.kill();
            let _ = child.wait();
            WslError::ProbeFailed(format!("bitty component: WSL probe thread failed: {error}"))
        })?;
    let stdout = match rx.recv_timeout(WSL_PROBE_TIMEOUT) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(WslError::ProbeFailed(format!(
                "bitty component: cannot read `{WSL_EXE} {arg}` output: {error}"
            )));
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(WslError::AbsentWsl(absent_diagnostic()));
        }
    };
    let status = child.wait().map_err(|error| {
        WslError::ProbeFailed(format!(
            "bitty component: cannot wait for `{WSL_EXE} {arg}`: {error}"
        ))
    })?;
    if stdout.len() > WSL_VERSION_OUTPUT_MAX_BYTES {
        return Err(WslError::ProbeFailed(format!(
            "bitty component: `{WSL_EXE} {arg}` output exceeds {WSL_VERSION_OUTPUT_MAX_BYTES} bytes"
        )));
    }
    Ok((status.success(), stdout))
}

/// Actionable diagnostic when WSL is absent (names the Store upgrade for
/// inbox builds, per the fail-closed posture).
#[must_use]
pub fn absent_diagnostic() -> String {
    "bitty component: WSL2 is unavailable (`wsl.exe --version` and `wsl.exe --status` both failed; install or enable WSL2 from the Microsoft Store (inbox builds lack `--version`) and add a guest distro, or run without --distro for native Windows execution)".to_string()
}

/// Detect the WSL context for trampoline execution.
///
/// On Windows this first probes `wsl.exe --version` (Store builds; success
/// returns `Ok(Some(version))`, first line trimmed). When that command is
/// unsupported (non-zero exit, older inbox builds) it falls back to
/// `wsl.exe --status` success as present-but-unversioned
/// (`Ok(Some("present (unversioned, --status ok)"))`; no version floor is
/// enforced, so proceeding is safe). Missing `wsl.exe` (`NotFound`) or both
/// probes failing returns [`WslError::AbsentWsl`] with [`absent_diagnostic`]
/// (exit `1`). Choosing fallback-over-fail-closed for `--version`-only
/// absence is deliberate and documented: inbox WSL2 is operational, just
/// unversioned; genuinely absent WSL still fails closed. Probe stdout is
/// decoded via [`decode_probe_output`] (UTF-8 or UTF-16LE). On Unix this is a
/// no-op returning `Ok(None)` with zero spawn.
#[cfg(windows)]
pub fn detect_wsl_context() -> Result<Option<String>, WslError> {
    // Preferred probe: Store `wsl.exe` understands `--version`.
    match run_probe_command("--version") {
        Ok((true, stdout)) => {
            let text = decode_probe_output(&stdout);
            let first = text.lines().next().unwrap_or("").trim().to_string();
            if first.is_empty() || first.contains('\0') {
                // Empty or NUL-bearing version after decode: treat as
                // unsupported output, fall through to `--status` rather than
                // failing closed on a present-but-odd build.
            } else {
                return Ok(Some(first));
            }
        }
        Ok((false, _)) => {
            // Non-zero exit (notably inbox `--version` unsupported): fall
            // through to `--status`. `NotFound` never reaches here (it
            // errors as `AbsentWsl` inside `run_probe_command`).
        }
        Err(error) => {
            // Missing `wsl.exe` fails closed at once; any other spawn/read
            // failure is already a typed error.
            return Err(error);
        }
    }
    // Fallback probe for inbox builds: `--status` success means present,
    // version unknown. Output is presence-only (ignored beyond success).
    match run_probe_command("--status") {
        Ok((true, _)) => Ok(Some("present (unversioned, --status ok)".to_string())),
        Ok((false, _)) => Err(WslError::AbsentWsl(absent_diagnostic())),
        Err(error) => Err(error),
    }
}

/// No-op WSL detection off Windows: native execution, zero spawn.
#[cfg(not(windows))]
pub fn detect_wsl_context() -> Result<Option<String>, WslError> {
    Ok(None)
}

/// Whether the trampoline is active for `selection` on this platform.
///
/// Unix always returns `false` (no-op elsewhere, even when a distro is
/// selected, so Unix behavior is 100% unchanged). Windows returns `true`
/// exactly when `selection` is `Some` (a distro was selected via explicit
/// flag or config).
#[must_use]
pub fn wsl_trampoline_active(selection: Option<&WslDistro>) -> bool {
    #[cfg(windows)]
    {
        selection.is_some()
    }
    #[cfg(not(windows))]
    {
        let _ = selection;
        false
    }
}

/// Map a Windows executable path plus version-dir to guest spellings.
///
/// Convenience for the broker spawn path: both inputs are Windows spellings,
/// both are mapped via [`windows_path_to_guest`]. Fail-closed on the first
/// unmappable input.
pub fn map_store_paths_for_guest(
    executable_windows: &Path,
    version_dir_windows: &Path,
) -> Result<(String, String), WslError> {
    let exe = executable_windows.to_string_lossy().into_owned();
    let dir = version_dir_windows.to_string_lossy().into_owned();
    let guest_exe = windows_path_to_guest(&exe)?;
    let guest_dir = windows_path_to_guest(&dir)?;
    // The guest helper and cwd must both be absolute with no `..` (store
    // paths never contain `..`; see `valid_helper_guest_path`).
    if !valid_helper_guest_path(&guest_exe) {
        return Err(WslError::InvalidHelper(format!(
            "invalid mapped helper guest path {guest_exe:?} (refusing to exec)"
        )));
    }
    if !valid_helper_guest_path(&guest_dir) {
        return Err(WslError::InvalidHelper(format!(
            "invalid mapped guest working directory {guest_dir:?} (refusing to exec)"
        )));
    }
    Ok((guest_exe, guest_dir))
}

/// Extract the UNC distro from a Windows path, if it is a WSL UNC spelling.
///
/// Returns `Some(distro)` for `\\wsl$\<distro>\...` and
/// `\\wsl.localhost\<distro>\...` (bare `\\wsl$\<distro>` included), else
/// `None` (drive-absolute or unmappable shapes carry no namespace). Pure,
/// testable on every platform.
#[must_use]
pub fn unc_distro_of_windows_path(windows: &str) -> Option<String> {
    for prefix in ["\\\\wsl$\\", "\\\\wsl.localhost\\"] {
        if let Some(rest) = windows.strip_prefix(prefix) {
            let distro = match rest.split_once(['\\', '/']) {
                Some((distro, _)) => distro,
                None => rest,
            };
            if distro.is_empty() {
                return None;
            }
            return Some(distro.to_owned());
        }
    }
    None
}

/// Bind a Windows store path to its launch distro (fail-closed on mismatch).
///
/// UNC paths (`\\wsl$\<distroA>\...`) name their source namespace; executing
/// the mapped `/...` under a different launch distro `B` would address B's
/// bytes, not the verified A's bytes. This fails closed: UNC requires an
/// explicit [`WslDistro::Named`] equal (case-sensitive) to the UNC distro;
/// [`WslDistro::Default`] with a UNC path fails (the default may differ).
/// Drive-absolute paths bind to any launch distro. Pure, testable everywhere.
pub fn check_unc_distro_binding(windows_path: &str, launch: &WslDistro) -> Result<(), WslError> {
    let Some(unc) = unc_distro_of_windows_path(windows_path) else {
        return Ok(());
    };
    match launch {
        WslDistro::Named(selected) if selected == &unc => Ok(()),
        WslDistro::Named(selected) => Err(WslError::InvalidHelper(format!(
            "WSL UNC path {windows_path:?} names distro {unc:?} but launch selects {selected:?} (refusing cross-distro exec; verified bytes are from {unc:?})"
        ))),
        WslDistro::Default => Err(WslError::InvalidHelper(format!(
            "WSL UNC path {windows_path:?} names distro {unc:?} but launch uses the default distro (explicitly select --distro {unc:?} to bind execution to the verified store)"
        ))),
    }
}

/// Whether an env name is secret-shaped and must never cross into the guest.
///
/// Case-insensitive suffix match on `_TOKEN`, `_KEY`, `_SECRET` (mirrors the
/// seed transport posture). Proxy/TLS/locale names never match, so
/// forwarding is unaffected.
#[must_use]
pub fn is_secret_wsl_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.ends_with("_TOKEN") || upper.ends_with("_KEY") || upper.ends_with("_SECRET")
}

/// Base name of a `WSLENV` entry (strips `/p`, `/u`, `/l`, `/w` suffixes).
///
/// `WSLENV` entries may carry flags (`NAME/p`, `NAME/u`); the secret check
/// applies to the base name, while construction preserves the full entry
/// verbatim.
fn wslenv_base_name(entry: &str) -> &str {
    entry.split('/').next().unwrap_or(entry)
}

/// Build the `WSLENV` value for one trampoline spawn.
///
/// `existing` is the caller's current `WSLENV` (from the hermetic env, may be
/// `None`); `forwarding` are the allowlisted names the broker sets on the
/// Windows `wsl.exe` process and wants the guest to receive. Result is
/// colon-separated `existing` (verbatim, suffixes preserved) plus each
/// non-secret forwarding name not already present (bare, no suffix).
/// Secret-shaped names are stripped from both lists, even if the caller
/// listed them. Pure, testable on every platform.
#[must_use]
pub fn build_wslenv_for_forwarding(existing: Option<&str>, forwarding: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    if let Some(list) = existing {
        for entry in list.split(':') {
            if entry.is_empty() {
                continue;
            }
            if is_secret_wsl_env_name(wslenv_base_name(entry)) {
                continue;
            }
            if !out.iter().any(|kept| kept == entry) {
                out.push(entry.to_owned());
            }
        }
    }
    for name in forwarding {
        if name == "WSLENV" {
            continue;
        }
        if is_secret_wsl_env_name(name) {
            continue;
        }
        if !out.iter().any(|kept| kept == name) {
            out.push(name.clone());
        }
    }
    out.join(":")
}

/// Persisted distro file path inside one component directory (`<root>/<name>/`).
#[must_use]
pub fn persisted_distro_path(component_dir: &Path) -> std::path::PathBuf {
    component_dir.join(WSL_DISTRO_FILE)
}

/// Read the persisted distro selection for one component directory.
///
/// Missing file means native execution (`Ok(None)`, first contact). A valid
/// `default` or distro name (trimmed) returns the selection; oversize,
/// non-UTF-8, empty, symlink, non-file, or invalid content fails closed as
/// [`WslError`] (never a silent native fallback when the file exists). Pure
/// filesystem read (no spawn), testable on every platform via tempdirs.
pub fn read_persisted_wsl_distro(component_dir: &Path) -> Result<Option<WslDistro>, WslError> {
    let path = persisted_distro_path(component_dir);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(WslError::InvalidHelper(format!(
                "refusing symlink distro file '{}'",
                path.display()
            )));
        }
        Ok(metadata) if metadata.is_file() => {
            if metadata.len() > WSL_DISTRO_FILE_MAX_BYTES {
                return Err(WslError::InvalidDistro(format!(
                    "persisted distro file '{}' exceeds {WSL_DISTRO_FILE_MAX_BYTES} bytes (corrupt; remove it or re-run with --distro)",
                    path.display()
                )));
            }
        }
        Ok(_) => {
            return Err(WslError::InvalidHelper(format!(
                "refusing non-file distro path '{}'",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(WslError::ProbeFailed(format!(
                "cannot inspect '{}': {error}",
                path.display()
            )));
        }
    }
    let bytes = std::fs::read(&path).map_err(|error| {
        WslError::ProbeFailed(format!("cannot read '{}': {error}", path.display()))
    })?;
    let text = String::from_utf8(bytes).map_err(|_| {
        WslError::InvalidDistro(format!(
            "persisted distro file '{}' is not UTF-8 (corrupt; remove it or re-run with --distro)",
            path.display()
        ))
    })?;
    let name = text.trim();
    if name.is_empty() {
        return Err(WslError::InvalidDistro(format!(
            "persisted distro file '{}' is empty (corrupt; remove it or re-run with --distro)",
            path.display()
        )));
    }
    if name == "default" {
        return Ok(Some(WslDistro::Default));
    }
    if !valid_wsl_distro(name) {
        return Err(WslError::InvalidDistro(format!(
            "persisted distro file '{}' holds invalid distro {name:?} (corrupt; remove it or re-run with --distro)",
            path.display()
        )));
    }
    Ok(Some(WslDistro::Named(name.to_owned())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distro_grammar_accepts_valid_names() {
        for valid in [
            "Ubuntu",
            "Ubuntu-22.04",
            "Debian",
            "fedora_39",
            "a",
            "a1",
            "My.Distro-1_2",
            // Dashes are legal, even an `--exec` substring: it stays a single
            // discrete argv element after `--distribution` (no shell), so it
            // can never become a flag.
            "Ubuntu--exec",
        ] {
            assert!(valid_wsl_distro(valid), "{valid}");
        }
    }

    #[test]
    fn distro_grammar_rejects_hostile_shapes() {
        for invalid in [
            "",
            "default",
            "-Ubuntu",
            ".Ubuntu",
            "Ubuntu/Debian",
            "Ubuntu\\Debian",
            "Ubuntu Debian",
            "Ubuntu;evil",
            "Ubuntu|evil",
            "Ubuntu&evil",
            "Ubuntu$evil",
            "Ubuntu`evil`",
            "Ubuntu$(evil)",
            "--distribution",
            "a/b",
            "..",
            "a..b/c",
            "Ubuntu\n",
            "Ubuntu\r",
            "Ub\tuntu",
            "Ub\0untu",
        ] {
            assert!(!valid_wsl_distro(invalid), "{invalid:?}");
        }
        assert!(!valid_wsl_distro(&"a".repeat(65)));
        assert!(valid_wsl_distro(&"a".repeat(64)));
    }

    #[test]
    fn select_prefers_explicit_over_env_and_maps_default() {
        assert_eq!(select_wsl_distro(None, None).expect("none"), None);
        assert_eq!(
            select_wsl_distro(Some("Ubuntu"), Some("Debian")).expect("explicit wins"),
            Some(WslDistro::Named("Ubuntu".to_string()))
        );
        assert_eq!(
            select_wsl_distro(None, Some("Debian")).expect("env"),
            Some(WslDistro::Named("Debian".to_string()))
        );
        assert_eq!(
            select_wsl_distro(Some("default"), Some("Debian")).expect("explicit default"),
            Some(WslDistro::Default)
        );
        assert_eq!(
            select_wsl_distro(None, Some("default")).expect("env default"),
            Some(WslDistro::Default)
        );
        assert!(select_wsl_distro(Some(""), None).is_err());
        assert!(select_wsl_distro(Some("../evil"), None).is_err());
        assert!(select_wsl_distro(None, Some("")).is_err());
        assert!(select_wsl_distro(Some("Ubuntu;evil"), None).is_err());
    }

    #[test]
    fn windows_drive_paths_map_to_mnt() {
        assert_eq!(
            windows_path_to_guest("C:\\Users\\you\\AppData\\x").expect("drive"),
            "/mnt/c/Users/you/AppData/x"
        );
        assert_eq!(
            windows_path_to_guest("c:/Users/you").expect("forward slash"),
            "/mnt/c/Users/you"
        );
        assert_eq!(windows_path_to_guest("D:\\").expect("root"), "/mnt/d");
        assert_eq!(
            windows_path_to_guest("C:\\a\\..\\b").expect("dotdot preserved"),
            "/mnt/c/a/../b"
        );
        assert_eq!(
            windows_path_to_guest("C:\\Program Files\\Bitty\\components").expect("spaces"),
            "/mnt/c/Program Files/Bitty/components"
        );
    }

    #[test]
    fn windows_unc_wsl_paths_map_to_guest_absolute() {
        assert_eq!(
            windows_path_to_guest("\\\\wsl$\\Ubuntu\\home\\you").expect("wsl unc"),
            "/home/you"
        );
        assert_eq!(
            windows_path_to_guest("\\\\wsl.localhost\\Debian\\etc\\hosts").expect("localhost"),
            "/etc/hosts"
        );
        assert_eq!(
            windows_path_to_guest("\\\\wsl$\\Ubuntu").expect("bare distro root"),
            "/"
        );
        assert_eq!(
            windows_path_to_guest("\\\\wsl$\\Ubuntu\\").expect("trailing slash"),
            "/"
        );
    }

    #[test]
    fn windows_unmappable_paths_fail_closed() {
        for bad in [
            "",
            "C:foo",
            "C:",
            "\\foo\\bar",
            "foo\\bar",
            "/home/you",
            "\\\\server\\share\\x",
            "\\\\?\\C:\\foo",
            "\\\\.\\C:\\foo",
            "\\\\wsl$\\\\home\\you",
            "\\\\wsl$\\..\\home",
            "C:\\foo\0bar",
        ] {
            assert!(windows_path_to_guest(bad).is_err(), "{bad:?}");
        }
        assert!(windows_path_to_guest(&("C:\\".to_string() + &"a".repeat(5000))).is_err());
    }

    #[test]
    fn guest_mnt_round_trips_drive_paths() {
        assert_eq!(
            guest_mnt_to_windows("/mnt/c/Users/you").expect("mnt"),
            "C:\\Users\\you"
        );
        assert_eq!(guest_mnt_to_windows("/mnt/c").expect("root"), "C:\\");
        assert_eq!(guest_mnt_to_windows("/mnt/d/").expect("trailing"), "D:\\");
        for bad in [
            "/home/you",
            "/etc/hosts",
            "/mnt",
            "/mnt/",
            "/mnt/cd/foo",
            "/mnt/C/foo",
            "/mnt/1/foo",
            "/mnt/c/foo\0bar",
            "",
            "mnt/c/foo",
            "C:\\foo",
        ] {
            assert!(guest_mnt_to_windows(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn argv_preserves_fixed_argv_with_no_shell() {
        let named = WslDistro::Named("Ubuntu".to_string());
        let argv = wsl_argv(
            &named,
            "/mnt/c/helper",
            &[
                "--foo".to_string(),
                "a b".to_string(),
                "--exec".to_string(),
                "; rm -rf /".to_string(),
            ],
        )
        .expect("argv");
        assert_eq!(
            argv,
            vec![
                "wsl.exe",
                "--distribution",
                "Ubuntu",
                "--exec",
                "/mnt/c/helper",
                "--foo",
                "a b",
                "--exec",
                "; rm -rf /",
            ]
        );
        let def = WslDistro::Default;
        let argv = wsl_argv(&def, "/helper", &[]).expect("default");
        assert_eq!(argv, vec!["wsl.exe", "--exec", "/helper"]);
        let argv = wsl_argv_with_cwd(
            &named,
            Some("/mnt/c/dir"),
            "/mnt/c/helper",
            &["x".to_string()],
        )
        .expect("cwd");
        assert_eq!(
            argv,
            vec![
                "wsl.exe",
                "--distribution",
                "Ubuntu",
                "--cd",
                "/mnt/c/dir",
                "--exec",
                "/mnt/c/helper",
                "x",
            ]
        );
        // Metacharacters are inert data (separate elements, no joining).
        assert!(
            argv.iter()
                .any(|element| element == "; rm -rf /" || element == "x")
        );
    }

    #[test]
    fn argv_rejects_bad_helpers() {
        let named = WslDistro::Named("Ubuntu".to_string());
        for bad in [
            "",
            "relative/helper",
            "C:\\Windows\\helper.exe",
            "/mnt/c/../evil",
            "/x/\0y",
        ] {
            assert!(
                wsl_argv(&named, bad, &[]).is_err(),
                "helper {bad:?} must fail"
            );
        }
        assert!(
            wsl_argv(
                &named,
                "/helper",
                &[String::from_utf8(vec![0]).unwrap_or_default()]
            )
            .is_err()
        );
        assert!(wsl_argv_with_cwd(&named, Some("relative"), "/helper", &[]).is_err());
    }

    #[test]
    fn trampoline_is_noop_off_windows() {
        let named = WslDistro::Named("Ubuntu".to_string());
        assert_eq!(wsl_trampoline_active(Some(&named)), cfg!(windows));
        assert!(!wsl_trampoline_active(None));
    }

    #[test]
    fn detect_is_noop_off_windows_with_zero_spawn() {
        // Pure no-op elsewhere: no spawn, native execution. On Windows this
        // probes `wsl.exe` and fails closed when WSL is absent; that live leg
        // is covered by the gated integration test, never here (skip with
        // notice instead of failing the suite).
        #[cfg(not(windows))]
        {
            let result = detect_wsl_context().expect("Unix detect is a no-op");
            assert_eq!(result, None);
        }
        #[cfg(windows)]
        {
            match detect_wsl_context() {
                Ok(_) => {}
                Err(error) => {
                    eprintln!(
                        "notice: WSL unavailable on this runner, skipping live probe ({})",
                        error.message()
                    );
                }
            }
        }
    }

    #[test]
    fn probe_output_decodes_utf8_and_utf16le() {
        assert_eq!(
            decode_probe_output(b"WSL version: 2.0.0\n"),
            "WSL version: 2.0.0\n"
        );
        // BOM-less UTF-16LE ASCII (`V\0 ...`).
        let mut utf16 = Vec::new();
        for byte in b"WSL 2" {
            utf16.push(*byte);
            utf16.push(0);
        }
        assert_eq!(decode_probe_output(&utf16), "WSL 2");
        // BOM-prefixed UTF-16LE.
        let mut bom = vec![0xFF, 0xFE];
        bom.extend(utf16);
        assert_eq!(decode_probe_output(&bom), "WSL 2");
        // Plain UTF-8 with no NULs passes through.
        assert_eq!(decode_probe_output(b""), "");
    }

    #[test]
    fn probe_decision_classifies_missing_present_fallback() {
        assert_eq!(
            ProbeDecision::classify(Some(std::io::ErrorKind::NotFound), false),
            ProbeDecision::Missing
        );
        assert_eq!(
            ProbeDecision::classify(Some(std::io::ErrorKind::PermissionDenied), false),
            ProbeDecision::SpawnFailed
        );
        assert_eq!(ProbeDecision::classify(None, true), ProbeDecision::Present);
        assert_eq!(
            ProbeDecision::classify(None, false),
            ProbeDecision::Fallback
        );
    }

    #[test]
    fn unc_distro_binding_rejects_cross_distro() {
        let a = WslDistro::Named("Ubuntu".to_string());
        let b = WslDistro::Named("Debian".to_string());
        assert!(check_unc_distro_binding("C:\\x\\y", &a).is_ok());
        assert!(check_unc_distro_binding("\\\\wsl$\\Ubuntu\\home\\you", &a).is_ok());
        assert!(check_unc_distro_binding("\\\\wsl$\\Ubuntu\\home\\you", &b).is_err());
        assert!(
            check_unc_distro_binding("\\\\wsl$\\Ubuntu\\home\\you", &WslDistro::Default).is_err()
        );
        assert_eq!(
            unc_distro_of_windows_path("\\\\wsl$\\Ubuntu\\home\\you").as_deref(),
            Some("Ubuntu")
        );
        assert_eq!(unc_distro_of_windows_path("C:\\x"), None);
    }

    #[test]
    fn wslenv_appends_without_secrets_and_preserves_existing() {
        // Existing entries preserved (suffixes verbatim), forwarding appended,
        // secrets stripped from both lists, `WSLENV` itself never listed.
        let built = build_wslenv_for_forwarding(
            Some("MYVAR/MYVAR2:p"),
            &[
                "HTTPS_PROXY".to_string(),
                "no_proxy".to_string(),
                "WSLENV".to_string(),
                "GITHUB_TOKEN".to_string(),
            ],
        );
        assert_eq!(built, "MYVAR/MYVAR2:p:HTTPS_PROXY:no_proxy");
        // Existing secret entries are stripped even when the caller listed them.
        let built =
            build_wslenv_for_forwarding(Some("MYVAR:GITHUB_TOKEN"), &["HTTPS_PROXY".to_string()]);
        assert_eq!(built, "MYVAR:HTTPS_PROXY");
        // No existing list: forwarding alone, proxy present.
        let built =
            build_wslenv_for_forwarding(None, &["HTTPS_PROXY".to_string(), "LANG".to_string()]);
        assert_eq!(built, "HTTPS_PROXY:LANG");
        assert!(is_secret_wsl_env_name("GITHUB_TOKEN"));
        assert!(is_secret_wsl_env_name("aws_secret_access_key"));
        assert!(!is_secret_wsl_env_name("HTTPS_PROXY"));
        assert!(!is_secret_wsl_env_name("WSLENV"));
    }

    #[test]
    fn persisted_distro_round_trips_and_fails_closed() {
        let dir = std::env::temp_dir().join(format!(
            "bitty-wsl-distro-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        // Missing means native.
        assert_eq!(read_persisted_wsl_distro(&dir).expect("missing"), None);
        // Valid names round-trip (written as the CLI writes them).
        for (contents, expected) in [
            ("Ubuntu\n", WslDistro::Named("Ubuntu".to_string())),
            ("default\n", WslDistro::Default),
            ("  Debian  \r\n", WslDistro::Named("Debian".to_string())),
        ] {
            std::fs::write(persisted_distro_path(&dir), contents).expect("write");
            assert_eq!(
                read_persisted_wsl_distro(&dir).expect("valid"),
                Some(expected),
                "{contents:?}"
            );
        }
        // Corrupt content fails closed (never silent native).
        for bad in ["", "\n", "../evil", "Ubuntu/Debian", &"a".repeat(200)] {
            std::fs::write(persisted_distro_path(&dir), bad).expect("write bad");
            assert!(
                read_persisted_wsl_distro(&dir).is_err(),
                "corrupt {bad:?} must fail"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
