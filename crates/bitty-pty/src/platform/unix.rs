//! Unix PTY backend wrapping `portable-pty` (ADR-0004 "Wrap" row).
//!
//! Everything upstream-specific stays inside this module:
//!
//! - **Direct exec, no shell.** The child is spawned from the argv vector via
//!   the platform exec path; upstream never routes through a shell because
//!   this wrapper never uses `CommandBuilder::new_default_prog`.
//! - **Inherited session environment with overrides plus graphics
//!   sanitization.** Children inherit the session environment by default
//!   (DEC-0017, Ghostty/Alacritty reference). Inherited markers claiming
//!   graphics capabilities bitty lacks (CTX-0194: `GHOSTTY_*`, `WEZTERM_*`,
//!   `KITTY_*`, `VTE_VERSION`, iTerm markers, `TERM_PROGRAM_VERSION`) are
//!   removed before exec; explicit builder entries (`TERM`, `COLORTERM`,
//!   `TERM_PROGRAM=bitty`, and caller additions) are applied after that
//!   removal and therefore win. Upstream unconditionally injects
//!   a single extra variable (`SHELL`, resolved from `$SHELL` or the password
//!   database); an explicitly allowlisted `SHELL` overrides that injection.
//! - **fd hygiene.** Upstream sets close-on-exec on both PTY descriptors at
//!   openpty time and resets inherited signal dispositions plus the session
//!   in the child before exec; the wrapper keeps those guarantees by using
//!   the standard spawn path unchanged.
//!
//! If `portable-pty` ever becomes unmaintained for more than twelve months
//! on this hot path, ADR-0004 rule 3 requires replacing it with an owned fork
//! vendored under `vendor/`; only this module would need mechanical changes.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use portable_pty::CommandBuilder;
use portable_pty::PtySize;
use portable_pty::native_pty_system;

use crate::builder::SpawnConfig;
use crate::error::PtyError;
use crate::platform::ExitStatus;

pub(crate) struct Master {
    inner: Box<dyn portable_pty::MasterPty + Send>,
    writer_taken: bool,
}

pub(crate) struct Child {
    inner: Box<dyn portable_pty::Child + Send + Sync>,
}

fn to_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Maximum open+spawn attempts in [`open_pty_and_spawn`]: the first try plus
/// bounded retries of the transient race below.
const MAX_SPAWN_ATTEMPTS: usize = 5;

/// Pause between spawn attempts: lets the transient kernel state settle
/// instead of hot-spinning. Bounded (four 5 ms pauses worst case) and paid
/// only on the already-failing path; the success path never sleeps.
const SPAWN_RETRY_PAUSE: Duration = Duration::from_millis(5);

/// FreeBSD `NO_PID` sentinel (CTX-1020): `tcgetpgrp` succeeds — it does not
/// fail — but reports this value when the terminal has no foreground process
/// group (`tcgetpgrp(3)`: "If there is no foreground process group,
/// `tcgetpgrp()` returns an invalid process ID"). The value is the kernel's
/// `NO_PID` (100000); FreeBSD's default `kern.pid_max` is 99999, so it can
/// never be a real process-group id. Linux and macOS report "none" as an
/// error instead (mapped to `None` by the upstream `pid > 0` check), so this
/// filter is FreeBSD-only: a blanket filter would misclassify real high pids
/// elsewhere (Linux `pid_max` reaches into the millions).
#[cfg(target_os = "freebsd")]
const FREEBSD_NO_PID: u32 = 100_000;

/// Whether a spawn-attempt `errno` is the known-transient FreeBSD
/// controlling-terminal race (CTX-1020).
///
/// The upstream child's `TIOCSCTTY` transiently reports `EPERM` or `ENOTTY`
/// under parallel-spawn load although permissions are fine: bsd-tier 10-04
/// (`EPERM`/sh), 10-05 (`EPERM`/cat), release 37749864578 (`ENOTTY`/sh) and
/// bsd-tier 37749059735 (`ENOTTY`/env) each failed a *different* spawn while
/// identical spawns succeeded milliseconds apart, and no other errno was
/// ever observed. Only bare `std::io::Error` payloads from `cmd.spawn()` are
/// considered — upstream contextual errors (`"failed to openpty: …"`,
/// `"Unable to spawn …"`) never match, so genuine configuration failures
/// still fail fast.
fn is_transient_spawn_errno(errno: Option<i32>) -> bool {
    matches!(
        errno,
        Some(code) if code == libc::ENOTTY || code == libc::EPERM
    )
}

pub(crate) fn open_pty_and_spawn(
    config: &SpawnConfig,
) -> Result<(Master, Child, Option<crate::tree::OwnedTree>), PtyError> {
    let mut attempt: usize = 0;
    loop {
        attempt += 1;
        match open_pty_and_spawn_once(config) {
            Ok(output) => return Ok(output),
            Err((_, transient)) if transient && attempt < MAX_SPAWN_ATTEMPTS => {
                std::thread::sleep(SPAWN_RETRY_PAUSE);
            }
            Err((err, _)) => return Err(err),
        }
    }
}

/// One open+spawn attempt: a flattened [`PtyError`] plus whether the failure
/// is the transient race described in [`is_transient_spawn_errno`] (retry)
/// or terminal (fail fast). Every retry re-opens a fresh PTY pair, so a
/// stale device is abandoned rather than reused.
fn open_pty_and_spawn_once(
    config: &SpawnConfig,
) -> Result<(Master, Child, Option<crate::tree::OwnedTree>), (PtyError, bool)> {
    let pair = native_pty_system()
        .openpty(to_size(config.cols, config.rows))
        .map_err(|err| (PtyError::flatten_upstream(err), false))?;

    let mut argv: Vec<OsString> = Vec::with_capacity(config.args.len() + 1);
    argv.push(config.program.clone());
    argv.extend(config.args.iter().cloned());

    let mut command = CommandBuilder::from_argv(argv);
    if let Some(cwd) = &config.cwd {
        command.cwd(cwd);
    }
    // Inherit the session environment by default (DEC-0017, Ghostty/Alacritty reference).
    //
    // CTX-0194: strip inherited graphics-fingerprint markers before applying
    // overrides. A bitty child launched from ghostty/kitty/wezterm would
    // otherwise inherit e.g. TERM_PROGRAM=ghostty, and term-DB probes (chafa)
    // match that entry over xterm-256color and emit Kitty-graphics APC that
    // bitty renders blank. Removal is fail-safe (unknown keys are kept) and
    // only touches the documented fingerprint list; PATH/HOME/TERM stay
    // inherited. Explicit builder entries (TERM, COLORTERM,
    // TERM_PROGRAM=bitty, caller additions) are applied after removal and win.
    for (key, _) in std::env::vars_os() {
        if crate::builder::should_strip_graphics_fingerprint(&key) {
            command.env_remove(&key);
        }
    }
    // Belt-and-braces for exact keys (harmless when absent; covers a marker
    // appearing between the scan above and exec).
    for key in crate::builder::GRAPHICS_FINGERPRINT_EXACT_KEYS {
        command.env_remove(key);
    }
    // Caller-requested inherited-environment removals (minimized editor env,
    // W-103 G-2): after the fingerprint strip, before explicit overrides so
    // an explicit builder entry for the same key still wins.
    for key in &config.env_remove {
        command.env_remove(key);
    }
    // Explicit builder entries override the (sanitized) inherited environment.
    for (key, value) in &config.env {
        command.env(key, value);
    }

    let child = match pair.slave.spawn_command(command) {
        Ok(child) => child,
        Err(err) => {
            // Classify before flattening: the retry decision needs the raw
            // errno behind the upstream error, which the flattened string
            // no longer carries. `downcast_ref` is inherent on the concrete
            // upstream error type, so no new dependency is named here.
            let transient = is_transient_spawn_errno(
                err.downcast_ref::<std::io::Error>()
                    .and_then(std::io::Error::raw_os_error),
            );
            // Drop the pair so no master/slave descriptor leaks on failure;
            // the whole pair is gone before any retry re-opens a fresh one.
            drop(pair.master);
            return Err((PtyError::flatten_upstream(err), transient));
        }
    };
    let child = Child { inner: child };

    // The child is a session leader from `fork`, so it already leads its own
    // process group: adopting its pid observes the tree without any race.
    // A missing pid (upstream reports none) simply means no tree.
    let tree = child_pid(&child).and_then(|pid| crate::tree::OwnedTree::adopt(pid).ok());

    Ok((
        Master {
            inner: pair.master,
            writer_taken: false,
        },
        child,
        tree,
    ))
}

pub(crate) fn resize(master: &Master, cols: u16, rows: u16) -> Result<(), PtyError> {
    master
        .inner
        .resize(to_size(cols, rows))
        .map_err(PtyError::flatten_upstream)
}

pub(crate) fn size(master: &Master) -> Result<(u16, u16), PtyError> {
    let measured = master
        .inner
        .get_size()
        .map_err(PtyError::flatten_upstream)?;
    Ok((measured.cols, measured.rows))
}

pub(crate) fn tty_name(master: &Master) -> Option<PathBuf> {
    master.inner.tty_name()
}

pub(crate) fn process_group_leader(master: &Master) -> Option<u32> {
    // CTX-0370: kernel foreground process-group leader (`tcgetpgrp` on the
    // master fd, wrapped by portable-pty). Non-positive pids mean "no
    // foreground group" and map to `None`.
    master
        .inner
        .process_group_leader()
        .and_then(|pid| u32::try_from(pid).ok())
        .and_then(filter_foreground_pgid)
}

/// Drops the FreeBSD `NO_PID` sentinel (see [`FREEBSD_NO_PID`]): "no
/// foreground group", never a job. Identity on every other platform.
#[cfg(target_os = "freebsd")]
fn filter_foreground_pgid(pid: u32) -> Option<u32> {
    (pid != FREEBSD_NO_PID).then_some(pid)
}

/// FreeBSD-only companion to the function below: identity on every other
/// platform (the `NO_PID` sentinel filter, CTX-1020, must stay FreeBSD-only).
#[cfg(not(target_os = "freebsd"))]
fn filter_foreground_pgid(pid: u32) -> Option<u32> {
    Some(pid)
}

pub(crate) fn take_reader(master: &mut Master) -> Result<Box<dyn io::Read + Send>, PtyError> {
    try_clone_reader(master)
}

fn try_clone_reader(master: &Master) -> Result<Box<dyn io::Read + Send>, PtyError> {
    master
        .inner
        .try_clone_reader()
        .map_err(PtyError::flatten_upstream)
}

pub(crate) fn take_writer(master: &mut Master) -> Result<Box<dyn io::Write + Send>, PtyError> {
    if master.writer_taken {
        return Err(PtyError::HalfAlreadyTaken("writer"));
    }
    let writer = master
        .inner
        .take_writer()
        .map_err(PtyError::flatten_upstream)?;
    master.writer_taken = true;
    Ok(writer)
}

pub(crate) fn child_pid(child: &Child) -> Option<u32> {
    child.inner.process_id()
}

pub(crate) fn kill(child: &mut Child) -> Result<(), PtyError> {
    child.inner.kill().map_err(PtyError::flatten_upstream)
}

pub(crate) fn try_wait(child: &mut Child) -> Result<Option<ExitStatus>, PtyError> {
    child
        .inner
        .try_wait()
        .map(|maybe| maybe.map(convert_status))
        .map_err(PtyError::flatten_upstream)
}

pub(crate) fn wait(child: &mut Child) -> Result<ExitStatus, PtyError> {
    child
        .inner
        .wait()
        .map(convert_status)
        .map_err(PtyError::flatten_upstream)
}

fn convert_status(status: portable_pty::ExitStatus) -> ExitStatus {
    ExitStatus {
        success: status.success(),
        code: status.exit_code(),
        signal: status.signal().map(std::convert::Into::into),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_spawn_errno_classifier_is_exact() {
        // CTX-1020: only the two errnos ever observed from the racy child
        // `TIOCSCTTY` retry; nearby failures (missing program, permission on
        // exec, no errno at all) must still fail fast.
        assert!(is_transient_spawn_errno(Some(libc::ENOTTY)));
        assert!(is_transient_spawn_errno(Some(libc::EPERM)));
        assert!(!is_transient_spawn_errno(Some(libc::ENOENT)));
        assert!(!is_transient_spawn_errno(Some(libc::EACCES)));
        assert!(!is_transient_spawn_errno(Some(libc::EIO)));
        assert!(!is_transient_spawn_errno(None));
    }

    #[cfg(target_os = "freebsd")]
    #[test]
    fn freebsd_no_pid_sentinel_is_not_a_foreground_group() {
        // CTX-1020: `tcgetpgrp` reports NO_PID while the shell has not taken
        // the foreground; that reading is "idle", never a job.
        assert_eq!(filter_foreground_pgid(FREEBSD_NO_PID), None);
        assert_eq!(filter_foreground_pgid(1), Some(1));
    }

    #[cfg(not(target_os = "freebsd"))]
    #[test]
    fn non_bsd_platforms_never_filter_a_foreground_pgid() {
        // CTX-1020: 100000 is a real pid elsewhere (Linux `pid_max` reaches
        // the millions), so the sentinel filter must stay FreeBSD-only.
        assert_eq!(filter_foreground_pgid(100_000), Some(100_000));
        assert_eq!(filter_foreground_pgid(1), Some(1));
    }
}
