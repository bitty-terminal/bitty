//! WSL2 trampoline for component binaries (issue #1908, part of #1902 delta 7).
//!
//! Windows component binaries that must run inside a WSL2 guest do so through
//! an explicit trampoline: the Windows host spawns `wsl.exe` (PATH-resolved,
//! fixed argv, no shell) which `exec`s the Linux helper inside the guest.
//! Unix hosts never use this module (all entry points are no-ops there, so
//! Unix behavior is 100% unchanged).
//!
//! # Guest distro selection
//!
//! - Explicit `--distro <name>` wins (CLI flag, `bitty component --help`).
//! - Else `BITTY_WSL_DISTRO` environment/config wins (`WslDistro::select` takes
//!   both as pure inputs; the caller reads the environment).
//! - Else the trampoline is disabled (native execution, `None`). There is no
//!   silent default: to run with the `wsl.exe` default distro pass the literal
//!   `default` (`--distro default` or `BITTY_WSL_DISTRO=default`), which maps
//!   to [`WslDistro::Default`] (no `--distribution` flag, `wsl.exe` picks its
//!   configured default).
//! - The literal `default` is reserved and never a distro name
//!   ([`valid_wsl_distro`] rejects it); any other invalid name fails closed as
//!   usage (exit `2`), never a silent pick.
//! - When a distro is selected but WSL is absent (no `wsl.exe` or the version
//!   probe fails), execution fails closed with an actionable diagnostic (exit
//!   `1`): install or enable WSL2, or run natively without `--distro`. There
//!   is no silent fallback to native when WSL was requested.
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
//! [`detect_wsl_context`] on Windows runs `wsl.exe --version` with a bounded
//! output cap ([`WSL_VERSION_OUTPUT_MAX_BYTES`]) and a bounded wait
//! ([`WSL_PROBE_TIMEOUT`]); absent `wsl.exe` or a failed probe returns
//! [`WslError::AbsentWsl`] with the manual-install diagnostic (exit `1`). On
//! Unix this function is a no-op returning `Ok(None)` with zero spawn.

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

/// Maximum bytes of `wsl.exe --version` output kept for the probe.
pub const WSL_VERSION_OUTPUT_MAX_BYTES: usize = 8192;

/// Bounded wait for the `wsl.exe --version` probe.
pub const WSL_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Detect the WSL context for trampoline execution.
///
/// On Windows this probes `wsl.exe --version` with a bounded output cap and
/// a bounded wait; success returns `Ok(Some(version))` (first line trimmed),
/// absence or failure returns [`WslError::AbsentWsl`] with the actionable
/// diagnostic (exit `1`). On Unix this is a no-op returning `Ok(None)` with
/// zero spawn (native execution, trampoline disabled).
#[cfg(windows)]
pub fn detect_wsl_context() -> Result<Option<String>, WslError> {
    let absent = || {
        WslError::AbsentWsl(
            "bitty component: WSL2 is unavailable (`wsl.exe --version` failed; install or enable WSL2 and a guest distro, or run without --distro for native Windows execution)".to_string(),
        )
    };
    let (tx, rx) = std::sync::mpsc::channel::<Result<std::process::Output, std::io::Error>>();
    std::thread::Builder::new()
        .name("bitty-wsl-probe".into())
        .spawn(move || {
            let output = std::process::Command::new(WSL_EXE)
                .arg("--version")
                .output();
            let _ = tx.send(output);
        })
        .map_err(|error| {
            WslError::ProbeFailed(format!("bitty component: WSL probe thread failed: {error}"))
        })?;
    let output = rx.recv_timeout(WSL_PROBE_TIMEOUT).map_err(|_| absent())?;
    let output = output.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            absent()
        } else {
            WslError::ProbeFailed(format!(
                "bitty component: cannot run `wsl.exe --version`: {error}"
            ))
        }
    })?;
    if !output.status.success() {
        return Err(absent());
    }
    if output.stdout.len() > WSL_VERSION_OUTPUT_MAX_BYTES {
        return Err(WslError::ProbeFailed(
            "bitty component: `wsl.exe --version` output exceeds bounds".to_string(),
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let first = text.lines().next().unwrap_or("").trim().to_string();
    if first.is_empty() {
        return Err(absent());
    }
    Ok(Some(first))
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
}
