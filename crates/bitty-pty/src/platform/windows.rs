//! Windows ConPTY backend over the native `bitty-winjob` spawn (CTX-0978,
//! DEC-0101).
//!
//! Same contract as [`super::unix`]: everything platform-specific stays
//! inside this module. The child is born inside its Job Object through
//! `PROC_THREAD_ATTRIBUTE_JOB_LIST`, so it runs zero instructions outside
//! the job: there is no adopt-after-start window (issue #1579). The
//! [`OwnedTree`](crate::tree::OwnedTree) returned with the session owns that
//! job; `bitty-runtime` kills through it instead of adopting the pid later.
//!
//! - **Direct exec, no shell.** The child is spawned from the argv vector;
//!   the command line is quoted per the MSVCRT rules and passed with a NULL
//!   application name, so `CreateProcessW` parses the module itself. Nothing
//!   here routes through a shell.
//! - **Inherited session environment with overrides plus graphics
//!   sanitization.** Children inherit the session environment by default
//!   (DEC-0017, Ghostty/Alacritty reference). Inherited markers claiming
//!   graphics capabilities bitty lacks (CTX-0194: `GHOSTTY_*`, `WEZTERM_*`,
//!   `KITTY_*`, `VTE_VERSION`, iTerm markers, `TERM_PROGRAM_VERSION`) are
//!   removed before exec; explicit builder entries (`TERM`, `COLORTERM`,
//!   `TERM_PROGRAM=bitty`, and caller additions) are applied after that
//!   removal and therefore win. Key matching is case-insensitive (Windows
//!   environment semantics), exactly as `portable-pty` 0.9 did.
//! - **No Unix-only surface.** ConPTY exposes no device path, so
//!   [`tty_name`](super::Session::tty_name) is always `None`. Exit statuses
//!   never carry a signal name: Windows has no POSIX signals, so
//!   [`ExitStatus::signal`](super::ExitStatus::signal) is always `None` and
//!   `kill` terminates via the platform equivalent of SIGKILL. Resize
//!   updates the ConPTY window size; there is no SIGWINCH delivery.
//!
//! # Divergence from `portable-pty` 0.9 (documented, not hidden)
//!
//! - `portable-pty` additionally merges machine-level registry entries
//!   (e.g. `SystemRoot`) required for child startup. This backend builds the
//!   environment block from the session environment only: in practice the
//!   session already carries those entries, and an explicitly allowlisted
//!   entry overrides any inherited value. A process launched with a stripped
//!   environment missing `SystemRoot` fails its spawn instead of silently
//!   gaining machine state.
//! - A bare program name resolves through `CreateProcessW`'s own search
//!   (application directory, system directories, `PATH` with `PATHEXT`)
//!   rather than a pre-resolved path. An explicitly set `cwd` that does not
//!   exist fails the spawn; it is not silently replaced with the home
//!   directory.
//! - `portable-pty` 0.9 (stale since 2025-02, past the ADR-0004 twelve-month
//!   rule) no longer backs this module on Windows. The Unix backend still
//!   wraps it; only this module needed mechanical changes, because no caller
//!   observes upstream types.

use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::PathBuf;

use bitty_winjob::{ChildSpec, ConPtyChild, ConPtyMaster, JobObject};

use crate::builder::SpawnConfig;
use crate::error::PtyError;
use crate::platform::ExitStatus;
use crate::tree::OwnedTree;

pub(crate) struct Master {
    inner: ConPtyMaster,
    /// Console size cache: the kernel offers no getter and resizes are
    /// synchronous, so this never drifts. `Cell` keeps the shared `resize`
    /// plumbing (`&Master`) intact.
    size: Cell<(u16, u16)>,
    writer_taken: bool,
}

pub(crate) struct Child {
    inner: ConPtyChild,
}

/// Exit code `Child::kill` terminates with: the code `std::process::Child::kill`
/// and `portable-pty` use on Windows, so a killed leader reports the same
/// status whichever path ended it.
const KILL_EXIT_CODE: u32 = 1;

pub(crate) fn open_pty_and_spawn(
    config: &SpawnConfig,
) -> Result<(Master, Child, Option<OwnedTree>), PtyError> {
    // The job must exist before the first instruction: the child joins it
    // atomically at creation, so a spawn failure here fails the whole spawn
    // (fail-closed) instead of producing a jobless child.
    let job = JobObject::new()?;
    let env = resolved_env(config);
    let spec = ChildSpec {
        program: &config.program,
        args: &config.args,
        env: &env,
        cwd: config.cwd.as_deref(),
        cols: config.cols,
        rows: config.rows,
    };
    let (master, child) = ConPtyMaster::spawn_in_job(&job, &spec)?;
    // The session's process handle pins the pid, so this open cannot reach
    // an unrelated recycled process. The member open is the only fallible
    // step after the spawn: on failure the job drops and kill-on-close ends
    // the orphan, so nothing leaks.
    let member = child.open_member()?;
    let pid = child.pid();
    let tree = OwnedTree::adopt_spawned(pid, job, member);
    Ok((
        Master {
            inner: master,
            size: Cell::new((config.cols, config.rows)),
            writer_taken: false,
        },
        Child { inner: child },
        Some(tree),
    ))
}

/// Final resolved environment: the session environment minus
/// graphics-fingerprint markers (CTX-0194) and caller removals, plus the
/// explicit builder overrides. Mirrors the `portable-pty` 0.9 policy this
/// backend replaces, including case-insensitive key matching.
fn resolved_env(config: &SpawnConfig) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    // Fingerprint strip (prefix match stays case-sensitive, as before).
    env.retain(|(key, _)| !crate::builder::should_strip_graphics_fingerprint(key));
    // Belt-and-braces exact keys, case-insensitive like the old path.
    for exact in crate::builder::GRAPHICS_FINGERPRINT_EXACT_KEYS {
        let exact = OsStr::new(exact);
        env.retain(|(key, _)| !keys_equal(key, exact));
    }
    // Caller-requested removals: after the fingerprint strip, before explicit
    // overrides so an explicit builder entry for the same key still wins.
    for key in &config.env_remove {
        env.retain(|(existing, _)| !keys_equal(existing, key));
    }
    // Explicit builder entries override the (sanitized) inherited environment.
    for (key, value) in &config.env {
        match env
            .iter_mut()
            .find(|(existing, _)| keys_equal(existing, key))
        {
            Some(slot) => slot.1.clone_from(value),
            None => env.push((key.clone(), value.clone())),
        }
    }
    env
}

/// Windows environment keys compare case-insensitively.
fn keys_equal(left: &OsStr, right: &OsStr) -> bool {
    left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
}

pub(crate) fn resize(master: &Master, cols: u16, rows: u16) -> Result<(), PtyError> {
    master.inner.resize(cols, rows).map_err(PtyError::Io)?;
    master.size.set((cols, rows));
    Ok(())
}

pub(crate) fn size(master: &Master) -> Result<(u16, u16), PtyError> {
    Ok(master.size.get())
}

pub(crate) fn tty_name(_master: &Master) -> Option<PathBuf> {
    // ConPTY exposes no terminal device path; there is nothing to report.
    None
}

pub(crate) fn process_group_leader(_master: &Master) -> Option<u32> {
    // CTX-0370: ConPTY exposes no process-group/foreground surface, so busy
    // detection is unavailable here and callers must treat `None` as
    // "cannot determine" (never as busy) rather than inventing a guess.
    None
}

pub(crate) fn take_reader(master: &mut Master) -> Result<Box<dyn io::Read + Send>, PtyError> {
    let handle = master.inner.take_reader().map_err(map_taken("reader"))?;
    // SAFETY: `handle` is a live, exclusively owned pipe end; `File` takes
    // sole ownership and closes it exactly once. No `unsafe` is involved:
    // `From<OwnedHandle> for File` is a safe conversion.
    Ok(Box::new(File::from(handle)))
}

pub(crate) fn take_writer(master: &mut Master) -> Result<Box<dyn io::Write + Send>, PtyError> {
    if master.writer_taken {
        return Err(PtyError::HalfAlreadyTaken("writer"));
    }
    let handle = master.inner.take_writer().map_err(map_taken("writer"))?;
    master.writer_taken = true;
    // SAFETY: same ownership argument as `take_reader`.
    Ok(Box::new(File::from(handle)))
}

/// Maps a backstop double-take (`AlreadyExists`) onto the typed half-taken
/// error; any other I/O failure travels as [`PtyError::Io`].
fn map_taken(what: &'static str) -> impl Fn(io::Error) -> PtyError {
    move |error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            PtyError::HalfAlreadyTaken(what)
        } else {
            PtyError::Io(error)
        }
    }
}

pub(crate) fn child_pid(child: &Child) -> Option<u32> {
    Some(child.inner.pid())
}

pub(crate) fn kill(child: &mut Child) -> Result<(), PtyError> {
    child.inner.kill(KILL_EXIT_CODE).map_err(PtyError::Io)
}

pub(crate) fn try_wait(child: &mut Child) -> Result<Option<ExitStatus>, PtyError> {
    child
        .inner
        .try_wait()
        .map(|maybe| maybe.map(convert_status))
        .map_err(PtyError::Io)
}

pub(crate) fn wait(child: &mut Child) -> Result<ExitStatus, PtyError> {
    child.inner.wait().map(convert_status).map_err(PtyError::Io)
}

fn convert_status(code: u32) -> ExitStatus {
    ExitStatus {
        // `portable-pty` parity: success is exactly exit code 0.
        success: code == 0,
        code,
        // Always `None` on Windows: ConPTY children exit with a code, never
        // with a POSIX signal.
        signal: None,
    }
}
