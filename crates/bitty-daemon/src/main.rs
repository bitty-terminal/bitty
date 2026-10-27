//! `bittyd` entry point: argument handling lives in [`bitty_daemon::cli`].
//!
//! The binary stays a thin wrapper: it owns no state and no I/O beyond
//! delegating to the library, so the IPC service task (bitty#2607) replaces the
//! seam inside the library without changing this file.

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    bitty_daemon::cli::run(std::env::args_os().skip(1))
}
