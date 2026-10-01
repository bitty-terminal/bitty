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
//!   (closed exactly once on drop) before anything else can fail.
//! - Every failure is `io::Error::last_os_error()` read immediately after
//!   the failing call, before any other system call can overwrite it.

use std::io;
use std::mem::size_of;
use std::os::windows::io::{
    AsHandle as _, AsRawHandle as _, BorrowedHandle, FromRawHandle as _, OwnedHandle,
};
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, FALSE, HANDLE, INVALID_HANDLE_VALUE, WAIT_FAILED,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
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
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, OpenThread, PROCESS_ACCESS_RIGHTS,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    ResumeThread, THREAD_SUSPEND_RESUME, TerminateProcess, WaitForSingleObject,
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
