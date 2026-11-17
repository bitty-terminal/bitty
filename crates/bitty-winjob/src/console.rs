//! Parent-console adoption for the GUI-subsystem `bitty` binary (bitty#2929).
//!
//! `bitty.exe` links as a Windows GUI-subsystem executable so a Start
//! Menu/Search launch never allocates a console window (Windows Terminal)
//! before the Bitty window appears. The cost of that choice is that the same
//! process also starts without a console when it *is* run from a shell, so
//! `bitty --version`, `--help`, `doctor`, `list`, `ctl`, interactive `init`,
//! and error output would have nowhere to read or write. Startup therefore
//! adopts the parent console when one exists (or keeps an already-attached
//! console when `AttachConsole` returns `ERROR_ACCESS_DENIED`).
//!
//! Only raw `AttachConsole`, `GetStdHandle`, and `SetStdHandle` calls live in
//! `crate::ffi`; the device handles come from `std::fs::OpenOptions`, so this
//! module adds no `unsafe` surface of its own.

#![forbid(unsafe_code)]

#[cfg(windows)]
use std::fs::{File, OpenOptions};
use std::io;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt as _;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle as _;

#[cfg(windows)]
use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
#[cfg(windows)]
use windows_sys::Win32::System::Console::{
    STD_ERROR_HANDLE, STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};

/// Console input device a GUI process reads from when it owns a console.
const CONSOLE_IN_DEVICE: &str = "CONIN$";

/// Console output device a GUI process writes to when it owns a console.
const CONSOLE_OUT_DEVICE: &str = "CONOUT$";

/// Null device: the destination for a stream that has neither a console nor
/// a parent-supplied handle.
const NULL_DEVICE: &str = "NUL";

/// Win32 `ERROR_ACCESS_DENIED`: `AttachConsole(ATTACH_PARENT_PROCESS)` fails
/// with this code when the calling process is already attached to a console
/// (for example via a console wrapper or inherited console).
#[cfg(windows)]
const ERROR_ACCESS_DENIED_CODE: i32 = i32::from_ne_bytes(ERROR_ACCESS_DENIED.to_ne_bytes());
#[cfg(not(windows))]
const ERROR_ACCESS_DENIED_CODE: i32 = 5;

/// Share flags required when opening `CONIN$` / `CONOUT$` (and compatible with
/// `NUL`): `FILE_SHARE_READ | FILE_SHARE_WRITE`, without `FILE_SHARE_DELETE`.
#[cfg(windows)]
const CONSOLE_SHARE_MODE: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;
#[cfg(not(windows))]
const CONSOLE_SHARE_MODE: u32 = 0x1 | 0x2;

#[cfg(windows)]
const _: () = {
    assert!(ERROR_ACCESS_DENIED_CODE == 5);
    assert!(CONSOLE_SHARE_MODE == 0x3);
};

/// Outcome of [`adopt_parent_console`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleStreams {
    /// A parent or inherited console is attached: missing streams now use it.
    Attached,
    /// No parent console (launched from Explorer/Search): missing streams
    /// use the null device.
    Detached,
}

impl ConsoleStreams {
    /// True when a console was adopted or already attached.
    #[must_use]
    pub fn attached(self) -> bool {
        matches!(self, Self::Attached)
    }
}

/// Logical standard stream adopted on Windows startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StandardStream {
    Input,
    Output,
    Error,
}

#[cfg(windows)]
impl StandardStream {
    const fn std_handle(self) -> STD_HANDLE {
        match self {
            Self::Input => STD_INPUT_HANDLE,
            Self::Output => STD_OUTPUT_HANDLE,
            Self::Error => STD_ERROR_HANDLE,
        }
    }
}

/// Device open parameters for one missing standard stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamAdoptSpec {
    stream: StandardStream,
    device: &'static str,
    read: bool,
    write: bool,
    share_mode: u32,
}

/// Classifies an `AttachConsole(ATTACH_PARENT_PROCESS)` result into
/// [`ConsoleStreams`].
///
/// `Ok(())` means the parent console was attached; `ERROR_ACCESS_DENIED` means
/// the process is already attached to a console (for example when launched
/// through a console wrapper). Any other error means there is no console to
/// adopt (`Detached`).
fn classify_attach_result(result: &io::Result<()>) -> ConsoleStreams {
    match result {
        Ok(()) => ConsoleStreams::Attached,
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED_CODE) => {
            ConsoleStreams::Attached
        }
        Err(_) => ConsoleStreams::Detached,
    }
}

/// Builds the ordered adoption specs for standard input, output, and error.
fn stream_specs(streams: ConsoleStreams) -> [StreamAdoptSpec; 3] {
    [
        StreamAdoptSpec {
            stream: StandardStream::Input,
            device: match streams {
                ConsoleStreams::Attached => CONSOLE_IN_DEVICE,
                ConsoleStreams::Detached => NULL_DEVICE,
            },
            read: true,
            write: true,
            share_mode: CONSOLE_SHARE_MODE,
        },
        StreamAdoptSpec {
            stream: StandardStream::Output,
            device: match streams {
                ConsoleStreams::Attached => CONSOLE_OUT_DEVICE,
                ConsoleStreams::Detached => NULL_DEVICE,
            },
            read: true,
            write: true,
            share_mode: CONSOLE_SHARE_MODE,
        },
        StreamAdoptSpec {
            stream: StandardStream::Error,
            device: match streams {
                ConsoleStreams::Attached => CONSOLE_OUT_DEVICE,
                ConsoleStreams::Detached => NULL_DEVICE,
            },
            read: true,
            write: true,
            share_mode: CONSOLE_SHARE_MODE,
        },
    ]
}

/// Adopts the parent console, when there is one, and makes the process
/// standard streams usable either way (bitty#2929).
///
/// - `AttachConsole(ATTACH_PARENT_PROCESS)` is the documented way for a GUI
///   process to share the console of the shell that started it; when it fails
///   with `ERROR_ACCESS_DENIED`, the process already owns/shares a console.
/// - A stream the parent already wired (a pipe, `> out.txt`, `< in.txt`, a CI
///   capture) is left untouched, so redirection and piping keep working.
/// - A stream that is missing is pointed at `CONIN$` (stdin) / `CONOUT$`
///   (stdout, stderr) when attached, and at `NUL` otherwise, opened read+write
///   with `FILE_SHARE_READ | FILE_SHARE_WRITE` (never `FILE_SHARE_DELETE`).
///
/// Best-effort by design: a failed device open or handle install leaves that
/// stream as it was. Never panics, never allocates a console.
#[cfg(windows)]
#[must_use]
pub fn adopt_parent_console() -> ConsoleStreams {
    let streams = classify_attach_result(&crate::ffi::attach_parent_console());
    let mut adopted: Vec<File> = Vec::new();
    for spec in stream_specs(streams) {
        let which = spec.stream.std_handle();
        if !crate::ffi::standard_stream_missing(which) {
            continue;
        }
        // `CONIN$`/`CONOUT$`/`NUL` are opened read+write with
        // `FILE_SHARE_READ | FILE_SHARE_WRITE` (without `FILE_SHARE_DELETE`):
        // that is the documented `CreateFile` form for console device handles.
        let Ok(file) = OpenOptions::new()
            .read(spec.read)
            .write(spec.write)
            .share_mode(spec.share_mode)
            .open(spec.device)
        else {
            continue;
        };
        if crate::ffi::set_standard_stream(which, file.as_raw_handle()).is_ok() {
            adopted.push(file);
        }
    }
    // `SetStdHandle` stores the handle without taking ownership, so every
    // adopted device must stay open for as long as anything may read or write
    // the stream. Leaking one handle per missing stream for the process
    // lifetime is the cheapest correct answer here; a `static` would need a
    // lock for the same lifetime.
    std::mem::forget(adopted);
    streams
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_result_treats_ok_and_access_denied_as_attached() {
        let ok = classify_attach_result(&Ok(()));
        assert_eq!(ok, ConsoleStreams::Attached);
        assert!(ok.attached());

        let already_attached =
            classify_attach_result(&Err(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED_CODE)));
        assert_eq!(already_attached, ConsoleStreams::Attached);
        assert!(already_attached.attached());
    }

    #[test]
    fn attach_result_treats_other_errors_as_detached() {
        for raw in [6, 31] {
            let detached = classify_attach_result(&Err(io::Error::from_raw_os_error(raw)));
            assert_eq!(detached, ConsoleStreams::Detached);
            assert!(!detached.attached());
        }

        let synthetic = classify_attach_result(&Err(io::Error::other("no parent console")));
        assert_eq!(synthetic, ConsoleStreams::Detached);
        assert!(!synthetic.attached());
    }

    #[test]
    fn stream_specs_map_handles_and_share_mode() {
        let attached = stream_specs(ConsoleStreams::Attached);
        assert_eq!(
            attached,
            [
                StreamAdoptSpec {
                    stream: StandardStream::Input,
                    device: "CONIN$",
                    read: true,
                    write: true,
                    share_mode: 0x3,
                },
                StreamAdoptSpec {
                    stream: StandardStream::Output,
                    device: "CONOUT$",
                    read: true,
                    write: true,
                    share_mode: 0x3,
                },
                StreamAdoptSpec {
                    stream: StandardStream::Error,
                    device: "CONOUT$",
                    read: true,
                    write: true,
                    share_mode: 0x3,
                },
            ]
        );

        let detached = stream_specs(ConsoleStreams::Detached);
        assert_eq!(
            detached,
            [
                StreamAdoptSpec {
                    stream: StandardStream::Input,
                    device: "NUL",
                    read: true,
                    write: true,
                    share_mode: 0x3,
                },
                StreamAdoptSpec {
                    stream: StandardStream::Output,
                    device: "NUL",
                    read: true,
                    write: true,
                    share_mode: 0x3,
                },
                StreamAdoptSpec {
                    stream: StandardStream::Error,
                    device: "NUL",
                    read: true,
                    write: true,
                    share_mode: 0x3,
                },
            ]
        );
    }
}
