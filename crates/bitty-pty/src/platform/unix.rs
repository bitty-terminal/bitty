//! Unix PTY backend wrapping `portable-pty` (ADR-0004 "Wrap" row).
//!
//! Everything upstream-specific stays inside this module:
//!
//! - **Direct exec, no shell.** The child is spawned from the argv vector via
//!   the platform exec path; upstream never routes through a shell because
//!   this wrapper never uses `CommandBuilder::new_default_prog`.
//! - **Minimal environment.** The builder's inherited environment is cleared
//!   before allowlisted entries are applied. Upstream unconditionally injects
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

pub(crate) fn open_pty_and_spawn(config: &SpawnConfig) -> Result<(Master, Child), PtyError> {
    let pair = native_pty_system()
        .openpty(to_size(config.cols, config.rows))
        .map_err(PtyError::flatten_upstream)?;

    let mut argv: Vec<OsString> = Vec::with_capacity(config.args.len() + 1);
    argv.push(config.program.clone());
    argv.extend(config.args.iter().cloned());

    let mut command = CommandBuilder::from_argv(argv);
    if let Some(cwd) = &config.cwd {
        command.cwd(cwd);
    }
    // Strip the inherited environment entirely, then forward exactly the
    // validated allowlist. See module docs for the single SHELL exception.
    command.env_clear();
    for (key, value) in &config.env {
        command.env(key, value);
    }

    let child = match pair.slave.spawn_command(command) {
        Ok(child) => child,
        Err(err) => {
            // Drop the pair so no master/slave descriptor leaks on failure.
            drop(pair.master);
            return Err(PtyError::flatten_upstream(err));
        }
    };

    Ok((
        Master {
            inner: pair.master,
            writer_taken: false,
        },
        Child { inner: child },
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

pub(crate) fn try_clone_reader(master: &Master) -> Result<Box<dyn io::Read + Send>, PtyError> {
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
