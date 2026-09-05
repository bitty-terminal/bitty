//! Windows ConPTY compile seam (NOT an implementation).
//!
//! ADR-0002 schedules Tier-1 Windows support for a later slice; CTX-0011
//! delivers the Unix PTY only. This module exists so that:
//!
//! - the crate's public API compiles unchanged on `cfg(windows)`;
//! - every operation fails loudly with [`PtyError::Unsupported`] instead of
//!   silently misbehaving or exposing a half-implemented ConPTY path;
//! - no Windows-specific unsafe, FFI, or ConPTY code is present yet.
//!
//! The future implementation must keep these exact signatures and honor the
//! same security defaults enforced on Unix: direct argv exec without shell
//! interpolation, inherited session environment with explicit overrides plus
//! CTX-0194 graphics-fingerprint sanitization, and bounded output buffering.

use std::io;
use std::path::PathBuf;

use crate::builder::SpawnConfig;
use crate::error::PtyError;
use crate::platform::ExitStatus;

// `Master` / `Child` are never constructed on Windows until the Tier-1
// Windows follow-up slice (ADR-0002) implements ConPTY. Keep the seam types
// constructible inside this module with a private sentinel so the crate
// continues to compile on `cfg(windows)` without emitting dead-code warnings
// that would mask future real usage.
#[allow(dead_code)]
pub(crate) struct Master {
    _private: (),
}

#[allow(dead_code)]
pub(crate) struct Child {
    _private: (),
}

const SEAM_MESSAGE: &str = "Windows ConPTY backend is not implemented yet; \
 it is planned for the Tier-1 Windows slice (ADR-0002)";

pub(crate) fn open_pty_and_spawn(config: &SpawnConfig) -> Result<(Master, Child), PtyError> {
    // `SpawnConfig` is validated on all platforms; on Windows the ConPTY
    // backend is not yet implemented (ADR-0002, Tier-1 Windows follow-up
    // slice) and this seam intentionally returns `Unsupported`. Touch the
    // config fields so they are considered read on `cfg(windows)` and the
    // seam remains exercisable without masking future real usage.
    let _ = (
        &config.program,
        &config.args,
        &config.env,
        &config.cwd,
        config.cols,
        config.rows,
    );
    Err(PtyError::Unsupported(SEAM_MESSAGE))
}

pub(crate) fn resize(_master: &Master, _cols: u16, _rows: u16) -> Result<(), PtyError> {
    Err(PtyError::Unsupported(SEAM_MESSAGE))
}

pub(crate) fn size(_master: &Master) -> Result<(u16, u16), PtyError> {
    Err(PtyError::Unsupported(SEAM_MESSAGE))
}

pub(crate) fn tty_name(_master: &Master) -> Option<PathBuf> {
    None
}

pub(crate) fn try_clone_reader(_master: &Master) -> Result<Box<dyn io::Read + Send>, PtyError> {
    Err(PtyError::Unsupported(SEAM_MESSAGE))
}

pub(crate) fn take_writer(_master: &mut Master) -> Result<Box<dyn io::Write + Send>, PtyError> {
    Err(PtyError::Unsupported(SEAM_MESSAGE))
}

pub(crate) fn child_pid(_child: &Child) -> Option<u32> {
    None
}

pub(crate) fn kill(_child: &mut Child) -> Result<(), PtyError> {
    Err(PtyError::Unsupported(SEAM_MESSAGE))
}

pub(crate) fn try_wait(_child: &mut Child) -> Result<Option<ExitStatus>, PtyError> {
    Err(PtyError::Unsupported(SEAM_MESSAGE))
}

pub(crate) fn wait(_child: &mut Child) -> Result<ExitStatus, PtyError> {
    Err(PtyError::Unsupported(SEAM_MESSAGE))
}
