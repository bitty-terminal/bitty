//! Every `unsafe` block of `bitty-winjob`: the raw Win32 calls behind the
//! safe [`crate::JobObject`] adapter (CTX-0903, DEC-0083).
//!
//! # Boundary rules (audited in `specifications/unsafe-ffi-audit.md`)
//!
//! - Inputs are only pids, exit codes, and handles this module created and
//!   owns as [`OwnedHandle`] (or borrows through [`BorrowedHandle`]); there
//!   is no parsing and no caller-controlled buffer.
//! - Every out-parameter is a `#[repr(C)]` `windows-sys` struct living on
//!   this function's stack, passed with its exact `size_of` length (or its
//!   `dwSize` field preset), so the kernel never writes past it.
//! - A raw `HANDLE` never leaves this module: each one is checked for its
//!   documented failure sentinel and wrapped into an [`OwnedHandle`]
//!   (closed exactly once on drop) before anything else can fail. The two
//!   exceptions own their non-`CloseHandle` cleanup instead: [`PseudoConsole`]
//!   (closed with `ClosePseudoConsole`) and [`AttributeList`] (deleted with
//!   `DeleteProcThreadAttributeList`); both run their cleanup exactly once
//!   from `Drop`.
//! - Every failure is `io::Error::last_os_error()` read immediately after
//!   the failing call, before any other system call can overwrite it. The
//!   pseudo-console calls report an `HRESULT` instead, which has no
//!   last-error contract, so those failures carry the `HRESULT` value itself.
//! - The ConPTY spawn path additionally takes caller-built UTF-16 buffers
//!   (command line, environment block, working directory). They are owned
//!   `Vec<u16>`s that outlive the `CreateProcessW` call, are `NUL`
//!   terminated by their builders, and their lengths are never passed: the
//!   kernel finds the terminators itself, so no length can be wrong.

use std::io;
use std::mem::size_of;
use std::os::windows::io::{
    AsHandle as _, AsRawHandle as _, BorrowedHandle, FromRawHandle as _, OwnedHandle,
};
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, FALSE, HANDLE, INVALID_HANDLE_VALUE, S_OK,
    WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, PSEUDOCONSOLE_INHERIT_CURSOR,
    ResizePseudoConsole,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess, OpenThread, PROC_THREAD_ATTRIBUTE_JOB_LIST,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_ACCESS_RIGHTS, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, THREAD_SUSPEND_RESUME, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

/// Rights a job member handle needs: `AssignProcessToJobObject` requires
/// `PROCESS_SET_QUOTA | PROCESS_TERMINATE`; exit observation needs
/// `SYNCHRONIZE` (wait) and `PROCESS_QUERY_LIMITED_INFORMATION` (exit code).
const MEMBER_RIGHTS: PROCESS_ACCESS_RIGHTS =
    PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;

/// Rights a liveness probe needs: wait plus nothing else.
const PROBE_RIGHTS: PROCESS_ACCESS_RIGHTS = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;

/// `WaitForSingleObject` timeout that polls without blocking.
const POLL_NOW_MS: u32 = 0;

/// `ResumeThread`'s failure sentinel (`(DWORD)-1`).
const RESUME_FAILED: u32 = u32::MAX;

/// Byte length of a `#[repr(C)]` information struct as the `u32` the Win32
/// calls take. Evaluated in `const` context at each use, so a struct that
/// could not be described fails the build instead of truncating.
const fn struct_len<T>() -> u32 {
    let len = size_of::<T>();
    assert!(len <= u32::MAX as usize, "Win32 struct length exceeds u32");
    len as u32
}

const EXTENDED_LIMIT_LEN: u32 = struct_len::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>();
const BASIC_ACCOUNTING_LEN: u32 = struct_len::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>();
const THREAD_ENTRY_LEN: u32 = struct_len::<THREADENTRY32>();

/// Wraps a handle returned by a call whose failure sentinel is NULL.
fn owned_or_null(raw: HANDLE) -> io::Result<OwnedHandle> {
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is non-NULL, so the call that produced it succeeded and
    // returned a fresh handle this process owns and nothing else references;
    // `OwnedHandle` takes sole ownership and closes it exactly once.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// Maps a Win32 `BOOL` result onto `io::Result`.
fn check(ok: windows_sys::core::BOOL) -> io::Result<()> {
    if ok == FALSE {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Creates an anonymous, non-inheritable job whose last handle close kills
/// every member (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`).
pub(crate) fn create_kill_on_close_job() -> io::Result<OwnedHandle> {
    // SAFETY: NULL security attributes (default descriptor, handle not
    // inheritable) and a NULL name (anonymous job) are documented valid
    // arguments; the returned handle is checked for NULL before use.
    let job = owned_or_null(unsafe { CreateJobObjectW(ptr::null(), ptr::null()) })?;
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `job` is a live job handle owned above; `limits` is a fully
    // initialized `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` on this stack frame
    // and the length passed is exactly its size, matching the
    // `JobObjectExtendedLimitInformation` class. The kernel only reads it.
    check(unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            ptr::from_ref(&limits).cast(),
            EXTENDED_LIMIT_LEN,
        )
    })?;
    Ok(job)
}

/// Opens `pid` with exactly the rights job membership and exit observation
/// need. The handle is not inheritable.
pub(crate) fn open_member(pid: u32) -> io::Result<OwnedHandle> {
    // SAFETY: plain value arguments; the result is checked for NULL.
    owned_or_null(unsafe { OpenProcess(MEMBER_RIGHTS, FALSE, pid) })
}

/// Adds `process` to `job`. Descendants it creates afterwards join too.
pub(crate) fn assign(job: BorrowedHandle<'_>, process: BorrowedHandle<'_>) -> io::Result<()> {
    // SAFETY: both handles are borrowed from live `OwnedHandle`s for the
    // duration of the call; `process` carries `PROCESS_SET_QUOTA |
    // PROCESS_TERMINATE` as the call requires (see `MEMBER_RIGHTS`).
    check(unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) })
}

/// Terminates every process currently in `job` with `exit_code`.
pub(crate) fn terminate_job(job: BorrowedHandle<'_>, exit_code: u32) -> io::Result<()> {
    // SAFETY: `job` is borrowed from a live job handle created with
    // `JOB_OBJECT_ALL_ACCESS` (the `CreateJobObjectW` default), which
    // includes `JOB_OBJECT_TERMINATE`.
    check(unsafe { TerminateJobObject(job.as_raw_handle(), exit_code) })
}

/// Number of processes currently alive in `job`.
pub(crate) fn active_processes(job: BorrowedHandle<'_>) -> io::Result<u32> {
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    // SAFETY: `job` is borrowed from a live job handle (default access
    // includes `JOB_OBJECT_QUERY`); `info` is a writable
    // `JOBOBJECT_BASIC_ACCOUNTING_INFORMATION` on this stack frame and the
    // length passed is exactly its size, matching the class; the optional
    // return-length out-parameter is NULL, which the API permits.
    check(unsafe {
        QueryInformationJobObject(
            job.as_raw_handle(),
            JobObjectBasicAccountingInformation,
            ptr::from_mut(&mut info).cast(),
            BASIC_ACCOUNTING_LEN,
            ptr::null_mut(),
        )
    })?;
    Ok(info.ActiveProcesses)
}

/// Whether the process behind `process` has exited, without consuming or
/// closing anything: `Ok(true)` once signaled, `Ok(false)` while running.
fn has_exited(process: BorrowedHandle<'_>) -> io::Result<bool> {
    // SAFETY: `process` is borrowed from a live handle opened with
    // `SYNCHRONIZE`; a zero timeout never blocks.
    match unsafe { WaitForSingleObject(process.as_raw_handle(), POLL_NOW_MS) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        WAIT_FAILED => Err(io::Error::last_os_error()),
        other => Err(io::Error::other(format!(
            "unexpected process wait result {other:#x}"
        ))),
    }
}

/// The exit code of the process behind `process` once it has exited, or
/// `None` while it runs. Non-reaping: Windows has no zombie to reap, and
/// the handle keeps the process object (and its pid) alive until closed.
pub(crate) fn exit_code(process: BorrowedHandle<'_>) -> io::Result<Option<u32>> {
    if !has_exited(process)? {
        return Ok(None);
    }
    let mut code = 0u32;
    // SAFETY: `process` is borrowed from a live handle opened with
    // `PROCESS_QUERY_LIMITED_INFORMATION`; `code` is a writable `u32` on
    // this stack frame.
    check(unsafe { GetExitCodeProcess(process.as_raw_handle(), &raw mut code) })?;
    Ok(Some(code))
}

/// Whether `pid` names a process that is still running. A pid with no
/// process object is `Ok(false)`.
pub(crate) fn process_is_running(pid: u32) -> io::Result<bool> {
    // SAFETY: plain value arguments; the NULL sentinel is checked below.
    let raw = unsafe { OpenProcess(PROBE_RIGHTS, FALSE, pid) };
    let process = match owned_or_null(raw) {
        Ok(process) => process,
        // `OpenProcess` reports a pid without a process object this way.
        Err(error) if error.raw_os_error() == Some(win32_code(ERROR_INVALID_PARAMETER)) => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    has_exited(process.as_handle()).map(|exited| !exited)
}

/// Terminates the single process `pid` with `exit_code`.
pub(crate) fn terminate_process(pid: u32, exit_code: u32) -> io::Result<()> {
    // SAFETY: plain value arguments; the NULL sentinel is checked.
    let process = owned_or_null(unsafe { OpenProcess(PROCESS_TERMINATE, FALSE, pid) })?;
    // SAFETY: `process` is a live handle opened with `PROCESS_TERMINATE`
    // just above and closed when it drops at the end of this function.
    check(unsafe { TerminateProcess(process.as_raw_handle(), exit_code) })
}

/// Resumes every thread `pid` owns once and returns how many were found.
pub(crate) fn resume_threads(pid: u32) -> io::Result<usize> {
    // SAFETY: plain value arguments (the pid is ignored for thread
    // snapshots); the `INVALID_HANDLE_VALUE` failure sentinel is checked
    // before the handle is wrapped.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh, valid snapshot handle (sentinel checked
    // above) that nothing else references; `OwnedHandle` closes it once.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut entry = THREADENTRY32 {
        dwSize: THREAD_ENTRY_LEN,
        ..THREADENTRY32::default()
    };
    // SAFETY: `snapshot` is a live thread snapshot; `entry` is a writable
    // `THREADENTRY32` on this stack frame whose `dwSize` is preset to its
    // exact size, as the API requires.
    let mut more = unsafe { Thread32First(snapshot.as_raw_handle(), &raw mut entry) };
    let mut resumed = 0usize;
    // A thread that fails to resume does not stop the walk: every other
    // thread of the process is still resumed, then the first error returns.
    let mut first_error = None;
    while more != FALSE {
        if entry.th32OwnerProcessID == pid {
            match resume_thread(entry.th32ThreadID) {
                Ok(()) => resumed += 1,
                // The thread exited between the snapshot and `OpenThread`:
                // there is nothing left to resume, so it is skipped.
                Err(error) if error.raw_os_error() == Some(win32_code(ERROR_INVALID_PARAMETER)) => {
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        // SAFETY: same snapshot and entry as above; the kernel keeps
        // `dwSize` and overwrites the remaining fields.
        more = unsafe { Thread32Next(snapshot.as_raw_handle(), &raw mut entry) };
    }
    // Read right after the failing `Thread32First`/`Thread32Next`.
    let end = io::Error::last_os_error();
    if let Some(error) = first_error {
        return Err(error);
    }
    if end.raw_os_error() != Some(win32_code(ERROR_NO_MORE_FILES)) {
        return Err(end);
    }
    Ok(resumed)
}

/// Decrements one thread's suspend count.
fn resume_thread(thread_id: u32) -> io::Result<()> {
    // SAFETY: plain value arguments; the NULL sentinel is checked.
    let thread = owned_or_null(unsafe { OpenThread(THREAD_SUSPEND_RESUME, FALSE, thread_id) })?;
    // SAFETY: `thread` is a live handle opened with
    // `THREAD_SUSPEND_RESUME` just above and closed when it drops.
    if unsafe { ResumeThread(thread.as_raw_handle()) } == RESUME_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A `WIN32_ERROR` as the `i32` `io::Error::raw_os_error` reports.
fn win32_code(code: u32) -> i32 {
    i32::from_ne_bytes(code.to_ne_bytes())
}

// ---------------------------------------------------------------------------
// ConPTY spawn path (CTX-0978, DEC-0101).
//
// Anonymous pipes, the pseudo-console, the process attribute list carrying
// the pseudo-console and the job list, and the `CreateProcessW` call that
// places the child in its job atomically at creation — closing the
// adopt-after-start window behind `OwnedTree::adopt` for ConPTY children.
// Every function below is `pub(crate)`: the safe `conpty` module owns the
// orchestration and the UTF-16 buffer builders, so the only values crossing
// this boundary are owned handles, borrowed handles, and NUL-terminated
// UTF-16 slices that outlive the call.
// ---------------------------------------------------------------------------

/// Pseudo-console flags matching the battle-tested `portable-pty` 0.9
/// selection: inherit the cursor, apply the resize quirk, and use Win32
/// input mode. `windows-sys` 0.61 names only the first, so the other two
/// are spelled out from the Windows SDK (`WinConTypes.h`):
/// `PSEUDOCONSOLE_RESIZE_QUIRK = 0x2`, `PSEUDOCONSOLE_WIN32_INPUT_MODE = 0x4`.
const PSEUDO_CONSOLE_FLAGS: u32 = PSEUDOCONSOLE_INHERIT_CURSOR | 0x2 | 0x4;

/// An open pseudo-console. Closed with `ClosePseudoConsole` on drop — never
/// `CloseHandle`, which would leak the console object.
pub(crate) struct PseudoConsole {
    hpcon: HPCON,
}

impl PseudoConsole {
    /// The raw console value for attribute-list registration. Stays valid
    /// while this value is alive; the spawn call must complete first.
    pub(crate) fn as_hpcon(&self) -> HPCON {
        self.hpcon
    }
}

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        // SAFETY: `hpcon` came from a successful `CreatePseudoConsole` and
        // this `Drop` runs exactly once per value; owners hold the console
        // across the spawn made with it, so it is alive for every use.
        unsafe { ClosePseudoConsole(self.hpcon) };
    }
}

/// Validates a console dimension: `COORD` carries `i16` lanes, so a `u16`
/// size that does not fit is refused instead of truncating.
fn console_lane(value: u16, what: &'static str) -> io::Result<i16> {
    i16::try_from(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("console {what} {value} exceeds the i16 COORD lane"),
        )
    })
}

/// Creates a pseudo-console of `cols` x `rows` cells attached to `input`
/// (read end) and `output` (write end). The console holds its own
/// references: like `portable-pty`, the caller drops the passed pipe ends
/// once this returns.
pub(crate) fn create_pseudo_console(
    cols: u16,
    rows: u16,
    input: BorrowedHandle<'_>,
    output: BorrowedHandle<'_>,
) -> io::Result<PseudoConsole> {
    let size = COORD {
        X: console_lane(cols, "width")?,
        Y: console_lane(rows, "height")?,
    };
    let mut hpcon: HPCON = 0;
    // SAFETY: `input`/`output` are borrowed from live pipe handles; `hpcon`
    // is a stack slot the kernel fills only on success; the `HRESULT` is
    // checked before use. Pseudo-console calls have no last-error contract,
    // so a failure carries the `HRESULT` itself.
    let result = unsafe {
        CreatePseudoConsole(
            size,
            input.as_raw_handle(),
            output.as_raw_handle(),
            PSEUDO_CONSOLE_FLAGS,
            &mut hpcon,
        )
    };
    if result != S_OK {
        return Err(io::Error::other(format!(
            "CreatePseudoConsole failed: HRESULT {result:#x}"
        )));
    }
    Ok(PseudoConsole { hpcon })
}

/// Resizes the console window to `cols` x `rows` cells.
pub(crate) fn resize_pseudo_console(
    console: &PseudoConsole,
    cols: u16,
    rows: u16,
) -> io::Result<()> {
    let size = COORD {
        X: console_lane(cols, "width")?,
        Y: console_lane(rows, "height")?,
    };
    // SAFETY: `console` is alive (borrowed); same `HRESULT` contract as
    // creation.
    let result = unsafe { ResizePseudoConsole(console.as_hpcon(), size) };
    if result != S_OK {
        return Err(io::Error::other(format!(
            "ResizePseudoConsole failed: HRESULT {result:#x}"
        )));
    }
    Ok(())
}

/// Creates one anonymous pipe with default buffering and non-inheritable
/// handles, returned as `(read, write)`.
pub(crate) fn create_pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read: HANDLE = ptr::null_mut();
    let mut write: HANDLE = ptr::null_mut();
    // SAFETY: both out-parameters are stack `HANDLE` slots; NULL attributes
    // (default descriptor, non-inheritable) and a zero buffer size are
    // documented valid. Both handles are wrapped before any fallible step.
    check(unsafe { CreatePipe(&mut read, &mut write, ptr::null(), 0) })?;
    // SAFETY: success checked above, so both handles are fresh, valid, and
    // exclusively owned; each is closed exactly once. No fallible step sits
    // between the two wraps, so neither can leak.
    let read = unsafe { OwnedHandle::from_raw_handle(read) };
    let write = unsafe { OwnedHandle::from_raw_handle(write) };
    Ok((read, write))
}

/// A process attribute list for one ConPTY spawn: exactly the pseudo-console
/// plus, optionally, one job. Deleted on drop.
pub(crate) struct AttributeList {
    buffer: Vec<u8>,
    /// Storage for the `PROC_THREAD_ATTRIBUTE_JOB_LIST` value: one job
    /// handle owned by this list, so its address is stable from the
    /// `UpdateProcThreadAttribute` call until `CreateProcessW` returns.
    job_slot: [HANDLE; 1],
}

impl AttributeList {
    /// Allocates a list for `attribute_count` attributes (1 or 2 here).
    pub(crate) fn new(attribute_count: u32) -> io::Result<Self> {
        if attribute_count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a process attribute list needs at least one attribute",
            ));
        }
        let mut bytes = 0usize;
        // SAFETY: a NULL list with a valid count only queries the required
        // size (documented to fail with `ERROR_INSUFFICIENT_BUFFER` while
        // writing it); no list exists yet, so nothing is freed. The return
        // value is intentionally unchecked: `bytes` is validated below.
        unsafe {
            InitializeProcThreadAttributeList(ptr::null_mut(), attribute_count, 0, &mut bytes)
        };
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0u8; bytes];
        // SAFETY: `buffer` is exactly the queried size and lives in `self`;
        // the return value is checked.
        check(unsafe {
            InitializeProcThreadAttributeList(
                buffer.as_mut_ptr().cast(),
                attribute_count,
                0,
                &mut bytes,
            )
        })?;
        Ok(Self {
            buffer,
            job_slot: [ptr::null_mut()],
        })
    }

    /// Raw list pointer for the spawn call. The buffer address is stable:
    /// nothing reallocates after `new`.
    fn as_ptr(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        // `Vec::as_ptr` never fails and the buffer outlives every borrow of
        // `self`, so the spawn call below cannot see a dangling pointer.
        self.buffer.as_ptr().cast_mut().cast()
    }

    /// Registers the pseudo-console on this list.
    pub(crate) fn set_pseudo_console(&mut self, console: &PseudoConsole) -> io::Result<()> {
        let hpcon = console.as_hpcon();
        // SAFETY: the list is live; per the `CreatePseudoConsole` contract
        // (and `portable-pty` 0.9) the `HPCON` travels by value in the
        // pointer slot with `size_of::<HPCON>()` — no dereferenceable memory
        // is involved and nothing outlives this call.
        check(unsafe {
            UpdateProcThreadAttribute(
                self.as_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                hpcon as *const core::ffi::c_void,
                size_of::<HPCON>(),
                ptr::null_mut(),
                ptr::null(),
            )
        })
    }

    /// Registers `job` so the spawned child joins it atomically at creation:
    /// the child runs zero instructions outside the job.
    pub(crate) fn set_job(&mut self, job: BorrowedHandle<'_>) -> io::Result<()> {
        self.job_slot[0] = job.as_raw_handle();
        let list = self.as_ptr();
        // SAFETY: `job_slot` is owned by `self` and `self` outlives the
        // spawn call (the caller holds it across `CreateProcessW`), so the
        // stored pointer stays valid; the size is exactly one `HANDLE` and
        // `job` is a live job handle.
        check(unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                self.job_slot.as_ptr().cast(),
                size_of::<[HANDLE; 1]>(),
                ptr::null_mut(),
                ptr::null(),
            )
        })
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: initialized by `new` and deleted exactly once here.
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) };
    }
}

/// A ConPTY child placed in its job at creation: the process handle that
/// observes (and terminates) it, plus its pid.
pub(crate) struct SpawnedConpty {
    pub(crate) process: OwnedHandle,
    pub(crate) pid: u32,
}

/// Spawns `command_line` attached to the pseudo-console (and job) in
/// `attributes`, exactly like `portable-pty` 0.9 but with a caller-built
/// attribute list: no shell, no handle inheritance, `STARTF_USESTDHANDLES`
/// with invalid stdio (the console is the stdio), and a caller-supplied
/// environment block. The primary thread handle is closed immediately; the
/// process handle is returned.
///
/// All buffers are NUL-terminated by their builders and owned by the caller
/// across this call; lengths are never passed (the kernel finds the
/// terminators), so no length can be wrong.
pub(crate) fn spawn_conpty(
    command_line: &mut [u16],
    environment: &[u16],
    current_directory: Option<&[u16]>,
    attributes: &AttributeList,
) -> io::Result<SpawnedConpty> {
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = struct_len::<STARTUPINFOEXW>();
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
    startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
    startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
    startup.lpAttributeList = attributes.as_ptr();
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: `command_line` is a writable NUL-terminated buffer the caller
    // keeps alive (the kernel may modify it while parsing); `environment`
    // and `current_directory` are NUL-terminated slices alive across the
    // call; `attributes` is a live two-entry list; `startup`/`info` are
    // stack structs with exact sizes. NULL application name is documented
    // valid (module name parsed from the command line) and disables handle
    // inheritance alongside `FALSE`.
    check(unsafe {
        CreateProcessW(
            ptr::null(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            FALSE,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            environment.as_ptr().cast(),
            current_directory.map_or(ptr::null(), |dir| dir.as_ptr()),
            ptr::from_ref(&startup.StartupInfo),
            &mut info,
        )
    })?;
    if info.hProcess.is_null() || info.hThread.is_null() || info.dwProcessId == 0 {
        // Practically unreachable (a successful `CreateProcessW` fills all
        // three), but fail closed without leaking: wrap whatever is valid
        // and drop it immediately.
        if !info.hThread.is_null() {
            // SAFETY: fresh handle from the call above, owned by nobody
            // else, closed once by the temporary.
            drop(unsafe { OwnedHandle::from_raw_handle(info.hThread) });
        }
        if !info.hProcess.is_null() {
            // SAFETY: same ownership argument as above.
            drop(unsafe { OwnedHandle::from_raw_handle(info.hProcess) });
        }
        return Err(io::Error::other(
            "CreateProcessW succeeded without process handles",
        ));
    }
    // SAFETY: success plus the NULL checks above make both handles fresh,
    // valid, and exclusively owned; the thread is closed at once (its work
    // is done) and the process exactly once by the returned owner.
    let thread = unsafe { OwnedHandle::from_raw_handle(info.hThread) };
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
    drop(thread);
    Ok(SpawnedConpty {
        process,
        pid: info.dwProcessId,
    })
}

/// Terminates the process behind `process` with `exit_code` through its
/// handle (no pid lookup, so no pid-reuse window).
pub(crate) fn terminate_handle(process: BorrowedHandle<'_>, exit_code: u32) -> io::Result<()> {
    // SAFETY: `process` is borrowed from a live full-access handle (the
    // `CreateProcessW` return), which includes `PROCESS_TERMINATE`.
    check(unsafe { TerminateProcess(process.as_raw_handle(), exit_code) })
}

/// Blocks until the process behind `process` exits, then reports its exit
/// code. Wakes when another thread terminates the process.
pub(crate) fn wait_for_exit(process: BorrowedHandle<'_>) -> io::Result<u32> {
    // SAFETY: `process` is borrowed from a live handle with `SYNCHRONIZE`;
    // an infinite wait ends when the process exits.
    let result = unsafe { WaitForSingleObject(process.as_raw_handle(), INFINITE) };
    if result == WAIT_FAILED {
        return Err(io::Error::last_os_error());
    }
    debug_assert_eq!(result, WAIT_OBJECT_0);
    let mut code = 0u32;
    // SAFETY: same handle (full access includes query rights); `code` is a
    // writable `u32` on this stack frame, read only after the wait above
    // signaled.
    check(unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) })?;
    Ok(code)
}
