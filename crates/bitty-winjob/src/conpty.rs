//! Safe ConPTY spawn over [`crate::ffi`]: the child joins its job atomically
//! at creation (CTX-0978, DEC-0101).
//!
//! `portable-pty` 0.9 spawns ConPTY children with fixed creation flags, so a
//! child assigned to a job afterwards runs its first instructions outside
//! it. This module replaces that spawn (and only that spawn) with a native
//! one built from the same recipe — anonymous pipes, `CreatePseudoConsole`,
//! `STARTF_USESTDHANDLES` with invalid stdio, no handle inheritance — plus a
//! two-entry process attribute list (pseudo-console and
//! `PROC_THREAD_ATTRIBUTE_JOB_LIST), so the child is born inside `job` and
//! runs zero instructions outside it.
//!
//! The UTF-16 builders below are pure safe functions; every `unsafe` block
//! they rely on stays in [`crate::ffi`]. No raw `HANDLE` or pointer crosses
//! this module's API: pipes and the process travel as [`OwnedHandle`], the
//! console as an RAII [`PseudoConsole`](crate::ffi::PseudoConsole) closed
//! with `ClosePseudoConsole`.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::io::{AsHandle as _, OwnedHandle};
use std::path::Path;

use crate::ffi;
use crate::job::{JobMember, JobObject};

/// Everything a ConPTY spawn needs, borrowed across the call.
pub struct ChildSpec<'a> {
    /// Program plus argument vector (direct argv, never a shell).
    pub program: &'a OsStr,
    /// Arguments after the program.
    pub args: &'a [OsString],
    /// Final resolved environment (session env minus removals plus
    /// overrides — the caller owns that policy).
    pub env: &'a [(OsString, OsString)],
    /// Working directory (`None` inherits).
    pub cwd: Option<&'a Path>,
    /// Initial console size in columns x rows.
    pub cols: u16,
    /// Initial console size in columns x rows.
    pub rows: u16,
}

/// A running ConPTY child born inside its job: the console, the master-side
/// pipe ends, and the process handle. Dropping every value closes its kernel
/// object; the job itself stays owned by the caller's [`JobObject`].
///
/// The spawn splits into a [`ConPtyMaster`] (console plus I/O) and a
/// [`ConPtyChild`] (process), mirroring the `bitty-pty` session halves.
pub struct ConPtyMaster {
    console: ffi::PseudoConsole,
    /// Master write end (bitty writes here); taken once by the writer half.
    input: Option<OwnedHandle>,
    /// Master read end (bitty reads here); taken once by the reader half.
    output: Option<OwnedHandle>,
}

/// The process half of a [`ConPtyMaster`] spawn.
pub struct ConPtyChild {
    /// Full-access process handle from `CreateProcessW`.
    process: OwnedHandle,
    pid: u32,
}

impl std::fmt::Debug for ConPtyMaster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConPtyMaster").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ConPtyChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConPtyChild")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl ConPtyMaster {
    /// Spawns `spec` attached to a fresh pseudo-console and placed in `job`
    /// atomically at creation. Buffers are built first so invalid input
    /// fails before any kernel object exists; on any later failure every
    /// object created so far drops (an unowned child is killed by the
    /// caller's job close, which owns the only job handle).
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] for malformed buffers (NUL,
    /// `=` in an env key, oversized console) and the system error when a
    /// kernel call refuses.
    pub fn spawn_in_job(job: &JobObject, spec: &ChildSpec<'_>) -> io::Result<(Self, ConPtyChild)> {
        let mut command_line = build_command_line(spec.program, spec.args)?;
        let environment = build_environment_block(spec.env)?;
        let current_directory = build_current_directory(spec.cwd)?;
        let (input_read, input_write) = ffi::create_pipe()?;
        let (output_read, output_write) = ffi::create_pipe()?;
        let console = ffi::create_pseudo_console(
            spec.cols,
            spec.rows,
            input_read.as_handle(),
            output_write.as_handle(),
        )?;
        // The console holds its own references from here on (same ownership
        // as `portable-pty` 0.9): the passed ends close now.
        drop(input_read);
        drop(output_write);
        let mut attributes = ffi::AttributeList::new(2)?;
        attributes.set_pseudo_console(&console)?;
        attributes.set_job(job.as_handle())?;
        let spawned = ffi::spawn_conpty(
            &mut command_line,
            &environment,
            current_directory.as_deref(),
            &attributes,
        )?;
        let master = Self {
            console,
            input: Some(input_write),
            output: Some(output_read),
        };
        let child = ConPtyChild {
            process: spawned.process,
            pid: spawned.pid,
        };
        Ok((master, child))
    }

    /// Resizes the console window; there is no SIGWINCH delivery. The caller
    /// tracks the size (the kernel offers no getter); resizes are
    /// synchronous, so the cache never drifts.
    ///
    /// # Errors
    ///
    /// Returns the system error when the console refuses.
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        ffi::resize_pseudo_console(&self.console, cols, rows)
    }

    /// Takes the master read end (once).
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::AlreadyExists`] when already taken.
    pub fn take_reader(&mut self) -> io::Result<OwnedHandle> {
        self.output.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "conpty reader half already taken",
            )
        })
    }

    /// Takes the master write end (once).
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::AlreadyExists`] when already taken.
    pub fn take_writer(&mut self) -> io::Result<OwnedHandle> {
        self.input.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "conpty writer half already taken",
            )
        })
    }
}

impl ConPtyChild {
    /// Process id of the child.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Opens the child as a job member for exit observation. The pid is
    /// pinned by this value's process handle, so it cannot have been
    /// recycled.
    ///
    /// # Errors
    ///
    /// Returns the system error when the process cannot be opened.
    pub fn open_member(&self) -> io::Result<JobMember> {
        Ok(JobMember::from_spawned(
            self.pid,
            ffi::open_member(self.pid)?,
        ))
    }

    /// Terminates the child with `exit_code` through its handle (no pid
    /// lookup, so no pid-reuse window). Does not reap; follow with
    /// [`wait`](Self::wait).
    ///
    /// # Errors
    ///
    /// Returns the system error when the kernel refuses.
    pub fn kill(&self, exit_code: u32) -> io::Result<()> {
        ffi::terminate_handle(self.process.as_handle(), exit_code)
    }

    /// Polls whether the child has exited without blocking: `None` while it
    /// runs. Never reaps; the open handle keeps the process object (and its
    /// pid) alive until this value drops.
    ///
    /// # Errors
    ///
    /// Returns the system error when the process cannot be waited on or
    /// queried.
    pub fn try_wait(&self) -> io::Result<Option<u32>> {
        ffi::exit_code(self.process.as_handle())
    }

    /// Blocks until the child exits and reports its exit code.
    ///
    /// # Errors
    ///
    /// Returns the system error when the wait or query fails.
    pub fn wait(&self) -> io::Result<u32> {
        ffi::wait_for_exit(self.process.as_handle())
    }
}

/// Builds the `CreateProcessW` command line: the program plus arguments
/// quoted per the MSVCRT rules (ported from `portable-pty` 0.9, via
/// rust-subprocess `ArgvQuote`). The result is NUL-terminated; the kernel
/// may modify the buffer while parsing, so the caller passes it by unique
/// borrow.
fn build_command_line(program: &OsStr, args: &[OsString]) -> io::Result<Vec<u16>> {
    let mut command_line = Vec::new();
    append_quoted(program, &mut command_line)?;
    for arg in args {
        command_line.push(' ' as u16);
        append_quoted(arg, &mut command_line)?;
    }
    command_line.push(0);
    Ok(command_line)
}

/// Quotes one argument with backslash/doubled-quote handling per
/// `CommandLineToArgvW` (an argument without spaces still travels verbatim).
fn append_quoted(arg: &OsStr, command_line: &mut Vec<u16>) -> io::Result<()> {
    let wide: Vec<u16> = arg.encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "command line argument must not contain NUL",
        ));
    }
    // Space, tab, newline, vertical tab, and double quote force quoting
    // (u16 code units: patterns cannot carry `as` casts).
    let needs_quotes = wide.is_empty()
        || wide
            .iter()
            .any(|unit| matches!(*unit, 32 | 9 | 10 | 11 | 34));
    if !needs_quotes {
        command_line.extend_from_slice(&wide);
        return Ok(());
    }
    command_line.push('"' as u16);
    let mut index = 0;
    while index < wide.len() {
        let mut backslashes = 0;
        while index < wide.len() && wide[index] == '\\' as u16 {
            index += 1;
            backslashes += 1;
        }
        if index == wide.len() {
            for _ in 0..backslashes * 2 {
                command_line.push('\\' as u16);
            }
            break;
        }
        if wide[index] == '"' as u16 {
            for _ in 0..backslashes * 2 + 1 {
                command_line.push('\\' as u16);
            }
            command_line.push(wide[index]);
        } else {
            for _ in 0..backslashes {
                command_line.push('\\' as u16);
            }
            command_line.push(wide[index]);
        }
        index += 1;
    }
    command_line.push('"' as u16);
    Ok(())
}

/// Builds the `CREATE_UNICODE_ENVIRONMENT` block (`key=value\0` entries plus
/// a final `\0`) from the resolved environment. Keys must be non-empty and
/// free of `=` and NUL; values free of NUL.
fn build_environment_block(env: &[(OsString, OsString)]) -> io::Result<Vec<u16>> {
    let mut block = Vec::new();
    for (key, value) in env {
        let key = wide_field(key, "environment key")?;
        let value = wide_field(value, "environment value")?;
        if key.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "environment key must not be empty",
            ));
        }
        if key.contains(&('=' as u16)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "environment key must not contain '='",
            ));
        }
        block.extend_from_slice(&key);
        block.push('=' as u16);
        block.extend_from_slice(&value);
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

/// Encodes one environment field, refusing NUL (which would truncate the
/// block at the kernel).
fn wide_field(field: &OsStr, what: &'static str) -> io::Result<Vec<u16>> {
    let wide: Vec<u16> = field.encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{what} must not contain NUL"),
        ));
    }
    Ok(wide)
}

/// Encodes the working directory (`None` inherits the caller's). A set but
/// empty or NUL-carrying path is refused instead of surprising
/// `CreateProcessW` (which would fail or inherit).
fn build_current_directory(cwd: Option<&Path>) -> io::Result<Option<Vec<u16>>> {
    let Some(dir) = cwd else {
        return Ok(None);
    };
    if dir.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "working directory must not be empty",
        ));
    }
    let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "working directory must not contain NUL",
        ));
    }
    wide.push(0);
    Ok(Some(wide))
}

#[cfg(test)]
mod tests {
    use std::os::windows::ffi::OsStringExt as _;

    use super::*;

    fn os(text: &str) -> OsString {
        OsString::from(text)
    }

    #[test]
    fn bare_words_travel_verbatim_with_single_nul() {
        let line =
            build_command_line(OsStr::new("cmd.exe"), &[os("/d"), os("/c")]).expect("command line");
        assert_eq!(line, "cmd.exe /d /c\0".encode_utf16().collect::<Vec<u16>>());
    }

    #[test]
    fn spaced_program_is_quoted() {
        let line = build_command_line(
            OsStr::new("C:\\Program Files\\tool.exe"),
            &[os("a b"), os("plain")],
        )
        .expect("command line");
        let text = String::from_utf16(&line[..line.len() - 1]).expect("utf16");
        assert_eq!(text, "\"C:\\Program Files\\tool.exe\" \"a b\" plain");
        assert_eq!(line.last(), Some(&0));
    }

    #[test]
    fn trailing_backslashes_are_doubled_before_the_closing_quote() {
        let line = build_command_line(OsStr::new("prog"), &[os("dir\\")]).expect("command line");
        let text = String::from_utf16(&line[..line.len() - 1]).expect("utf16");
        assert_eq!(text, "prog \"dir\\\\\"");
    }

    #[test]
    fn embedded_quotes_are_escaped() {
        let line =
            build_command_line(OsStr::new("prog"), &[os("say \"hi\"")]).expect("command line");
        let text = String::from_utf16(&line[..line.len() - 1]).expect("utf16");
        assert_eq!(text, "prog \"say \\\"hi\\\"\"");
    }

    #[test]
    fn nul_in_arg_is_refused() {
        let bad = OsString::from_wide(&['a' as u16, 0, 'b' as u16]);
        let error = build_command_line(OsStr::new("prog"), &[bad]).expect_err("NUL refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn environment_block_is_double_nul_terminated() {
        let block =
            build_environment_block(&[(os("A"), os("1")), (os("B"), os("2"))]).expect("block");
        let text = String::from_utf16(&block).expect("utf16");
        assert_eq!(text, "A=1\0B=2\0\0");
    }

    #[test]
    fn empty_environment_is_a_single_double_nul() {
        let block = build_environment_block(&[]).expect("block");
        assert_eq!(block, vec![0, 0]);
    }

    #[test]
    fn bad_environment_entries_are_refused() {
        for (key, value) in [
            (os(""), os("v")),
            (os("A=B"), os("v")),
            (os("K"), OsString::from_wide(&['v' as u16, 0])),
        ] {
            let error = build_environment_block(&[(key, value)]).expect_err("refused");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn unset_cwd_inherits() {
        assert_eq!(build_current_directory(None).expect("inherit"), None);
    }

    #[test]
    fn set_cwd_is_nul_terminated() {
        let dir = build_current_directory(Some(Path::new("C:\\work"))).expect("cwd");
        let dir = dir.expect("set");
        assert_eq!(dir.last(), Some(&0));
        assert_eq!(
            String::from_utf16(&dir[..dir.len() - 1]).expect("utf16"),
            "C:\\work"
        );
    }
}
