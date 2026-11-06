//! Parent-console adoption for the GUI-subsystem `bitty` binary (bitty#2929).
//!
//! `bitty.exe` links as a Windows GUI-subsystem executable so a Start
//! Menu/Search launch never allocates a console window (Windows Terminal)
//! before the Bitty window appears. The cost of that choice is that the same
//! process also starts without a console when it *is* run from a shell, so
//! `bitty --version`, `--help`, `doctor`, `list`, `ctl`, and error output
//! would print nowhere. Startup therefore adopts the parent console when one
//! exists.
//!
//! Only two raw calls live in [`crate::ffi`] (`AttachConsole` and
//! `SetStdHandle`); the device handles come from `std::fs::OpenOptions`, so
//! this module adds no new `unsafe` surface of its own.

use std::fs::{File, OpenOptions};
use std::os::windows::io::AsRawHandle as _;

use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};

/// Console device a GUI process writes to when it owns a console.
const CONSOLE_OUT_DEVICE: &str = "CONOUT$";

/// Null device: the destination for a stream that has neither a console nor
/// a parent-supplied handle.
const NULL_DEVICE: &str = "NUL";

/// Outcome of [`adopt_parent_console`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleStreams {
    /// A parent console was attached: missing streams now write to it.
    Attached,
    /// No parent console (launched from Explorer/Search): missing streams
    /// write to the null device.
    Detached,
}

impl ConsoleStreams {
    /// True when a parent console was adopted.
    #[must_use]
    pub fn attached(self) -> bool {
        matches!(self, Self::Attached)
    }
}

/// Adopts the parent console, when there is one, and makes the process
/// standard streams writable either way (bitty#2929).
///
/// - `AttachConsole(ATTACH_PARENT_PROCESS)` is the documented way for a GUI
///   process to share the console of the shell that started it.
/// - A stream the parent already wired (a pipe, `> out.txt`, a CI capture) is
///   left untouched, so redirection and piping keep working.
/// - A stream that is missing is pointed at `CONOUT$` when the attach
///   succeeded, and at `NUL` otherwise: `println!` on a null handle fails and
///   would panic, and a launcher start keeps its durable diagnostics in the
///   XDG crash-report slot instead of a console.
///
/// Best-effort by design: a failed device open or handle install leaves that
/// stream as it was. Never panics, never allocates a console.
#[must_use]
pub fn adopt_parent_console() -> ConsoleStreams {
    let attached = crate::ffi::attach_parent_console().is_ok();
    let device = if attached {
        CONSOLE_OUT_DEVICE
    } else {
        NULL_DEVICE
    };
    let mut adopted: Vec<File> = Vec::new();
    for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        if !crate::ffi::standard_stream_missing(which) {
            continue;
        }
        // `CONOUT$`/`NUL` are opened read+write: that is the documented
        // `CreateFile` form for a console output handle, and it keeps a single
        // device handle valid for both standard streams.
        let Ok(file) = OpenOptions::new().read(true).write(true).open(device) else {
            continue;
        };
        if crate::ffi::set_standard_stream(which, file.as_raw_handle()).is_ok() {
            adopted.push(file);
        }
    }
    // `SetStdHandle` stores the handle without taking ownership, so every
    // adopted device must stay open for as long as anything may write to the
    // stream. Leaking one handle per missing stream for the process lifetime
    // is the cheapest correct answer here; a `static` would need a lock for
    // the same lifetime.
    std::mem::forget(adopted);
    if attached {
        ConsoleStreams::Attached
    } else {
        ConsoleStreams::Detached
    }
}
