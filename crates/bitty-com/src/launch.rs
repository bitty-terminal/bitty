//! Resolve the sibling executable and start it in the classified mode.
//!
//! The sibling is always `bitty.exe` **in the directory of the running proxy**,
//! derived from [`std::env::current_exe`]. Nothing here reads `$PATH`, and no
//! install directory is hardcoded: the archive ships both files at its root, so
//! "next to me" is the whole contract.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::grammar::Mode;

/// Exit code when the GUI binary cannot be resolved or started at all.
///
/// Distinct from any child exit code the CLI produces: the shell sees a plain
/// failure instead of a silently detached process that never reported anything.
pub const EXIT_LAUNCH_FAILED: i32 = 1;

/// File name of the GUI sibling, in the same directory as the proxy.
#[cfg(windows)]
pub const SIBLING_FILE_NAME: &str = "bitty.exe";

/// File name of the GUI sibling, in the same directory as the proxy.
///
/// Non-Windows builds exist only so the crate compiles and can be exercised by
/// hand on a Linux checkout; there the shipped binary carries no extension.
#[cfg(not(windows))]
pub const SIBLING_FILE_NAME: &str = "bitty";

/// The sibling path for `proxy_exe`: its own directory plus
/// [`SIBLING_FILE_NAME`].
///
/// Returns `None` when `proxy_exe` has no directory component to anchor to.
#[must_use]
pub fn sibling_of(proxy_exe: &Path) -> Option<PathBuf> {
    let directory = proxy_exe.parent()?;
    Some(directory.join(SIBLING_FILE_NAME))
}

/// Classify this process's argv, resolve the sibling, and run it.
///
/// Returns the exit code for this process: the child's own code in
/// [`Mode::WaitCli`], `0` in [`Mode::DetachGui`], or [`EXIT_LAUNCH_FAILED`]
/// when the GUI binary could not be resolved or started.
#[must_use]
pub fn run() -> i32 {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let proxy = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("bitty.com: cannot resolve this executable's own path: {error}");
            return EXIT_LAUNCH_FAILED;
        }
    };
    run_from(&proxy, &args)
}

/// [`run`] with the proxy path and argv supplied by the caller.
///
/// Exposed so the classification and resolution steps are exercised by tests
/// without a real console; the process plumbing stays in [`run`].
#[must_use]
pub fn run_from(proxy: &Path, args: &[String]) -> i32 {
    let mode = crate::grammar::classify_with_env(args, crate::grammar::headless_requested_by_env());
    let Some(target) = sibling_of(proxy) else {
        eprintln!(
            "bitty.com: cannot derive a sibling directory from {}",
            proxy.display()
        );
        return EXIT_LAUNCH_FAILED;
    };
    if !target.is_file() {
        eprintln!(
            "bitty.com: {} is missing; reinstall Bitty so bitty.exe ships next to bitty.com",
            target.display()
        );
        return EXIT_LAUNCH_FAILED;
    }
    match spawn(&target, args, mode) {
        Ok(Some(code)) => code,
        Ok(None) => 0,
        Err(error) => {
            eprintln!("bitty.com: cannot start {}: {error}", target.display());
            EXIT_LAUNCH_FAILED
        }
    }
}

/// Spawn `target` with `args`.
///
/// [`Mode::WaitCli`] inherits the console streams and waits; [`Mode::DetachGui`]
/// nulls them, applies the platform detach flags, and returns without waiting
/// (the child keeps running after this process exits).
fn spawn(target: &Path, args: &[String], mode: Mode) -> std::io::Result<Option<i32>> {
    let mut command = Command::new(target);
    command.args(args);
    match mode {
        Mode::WaitCli => {
            command
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            let mut child = command.spawn()?;
            let status = child.wait()?;
            Ok(Some(status.code().unwrap_or(EXIT_LAUNCH_FAILED)))
        }
        Mode::DetachGui => {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            detach(&mut command);
            command.spawn()?;
            Ok(None)
        }
    }
}

/// `DETACHED_PROCESS` (`processthreadsapi.h`): the child gets no console.
///
/// Spelled here rather than imported because `std` exposes
/// [`std::os::windows::process::CommandExt::creation_flags`] but not the
/// individual `CreateProcess` flag constants; the value is the documented Win32
/// ABI constant and needs no dependency.
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// `CREATE_NEW_PROCESS_GROUP` (`processthreadsapi.h`): the child owns its own
/// console control group. Same rationale as [`DETACHED_PROCESS`].
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Apply the Windows detach flags to a child command.
#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS: the GUI child must not inherit (or allocate) a console,
    // so its window is the only surface. CREATE_NEW_PROCESS_GROUP: a Ctrl+C in
    // the launching shell is never delivered to the detached window.
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

/// Non-Windows fallback: there is no detach flag to set.
#[cfg(not(windows))]
fn detach(_command: &mut Command) {
    // POSIX has no equivalent; null stdio plus returning without waiting gives
    // the same contract for manual runs on a developer machine.
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{SIBLING_FILE_NAME, sibling_of};

    #[test]
    fn sibling_lives_in_the_proxy_directory() {
        let proxy = Path::new("/opt/bitty/bin/bitty.com");
        let sibling = sibling_of(proxy).expect("absolute proxy path has a parent");
        assert_eq!(sibling, Path::new("/opt/bitty/bin").join(SIBLING_FILE_NAME));
        assert_eq!(sibling.parent(), proxy.parent());
    }

    #[test]
    fn sibling_never_consults_path_or_a_hardcoded_root() {
        // A bare name resolves relative to the invocation, never through $PATH
        // and never against an install directory baked into this source.
        let sibling = sibling_of(Path::new("bitty.com")).expect("bare name keeps its tail");
        assert_eq!(sibling, Path::new(SIBLING_FILE_NAME));
        assert!(!sibling.is_absolute());
    }

    #[test]
    fn windows_sibling_is_the_gui_executable() {
        #[cfg(windows)]
        assert_eq!(SIBLING_FILE_NAME, "bitty.exe");
        #[cfg(not(windows))]
        assert_eq!(SIBLING_FILE_NAME, "bitty");
    }
}
