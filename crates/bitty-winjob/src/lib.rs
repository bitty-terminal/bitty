//! `bitty-winjob`: the narrow, reviewed Win32 adapter behind Bitty's Windows
//! owned-process-tree backend (CTX-0903, DEC-0083; detached lifetime
//! CTX-0997, DEC-0102) and the parent-console adoption of the GUI-subsystem
//! `bitty` binary (bitty#2929).
//!
//! This crate is to Windows what `rustix` (Linux) and `nix` (macOS) are to
//! `bitty-pty`'s owned-tree backends: the one place system calls are made,
//! wrapped in a safe API. Every `unsafe` block lives in the private `ffi`
//! module, each with a `SAFETY` rationale, and is inventoried in the
//! [evidence-matrix R-018 row](https://github.com/bitty-terminal/bitty-docs/blob/main/docs/security/evidence-matrix.md).
//! The crate root denies
//! `unsafe_code`; only that module is allowed it.
//!
//! # Surface (Windows only)
//!
//! - `JobObject`: an anonymous job, kill-on-close by default
//!   (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`). `JobObject::assign_pid` adds
//!   a process (its later descendants join automatically) and returns a
//!   `JobMember` for non-reaping exit observation;
//!   `JobObject::terminate` ends every member; dropping a kill-on-close
//!   job closes its only handle, which makes the kernel kill every
//!   remaining member. `JobObject::new_detached` skips the limit so
//!   detached and service trees outlive this process (DEC-0102).
//! - `resume_suspended_process`: resumes a process created with
//!   `CREATE_SUSPENDED_FLAG`, so a caller can assign it to a job before
//!   it runs a single instruction (no descendant can escape the job).
//! - `ConPtyMaster` plus `ConPtyChild`: a ConPTY child spawned directly
//!   into a `JobObject` through `PROC_THREAD_ATTRIBUTE_JOB_LIST`
//!   (CTX-0978, DEC-0101), so it runs zero instructions outside the job —
//!   no adopt-after-start window at all.
//! - `process_is_running` and `terminate_process`: single-pid helpers
//!   for liveness probes and last-resort cleanup.
//! - `adopt_parent_console`: attaches the GUI-subsystem `bitty` binary to the
//!   console of the shell that started it and points missing standard streams
//!   at that console (or at `NUL` when there is none), so `bitty --version`,
//!   `--help`, `doctor`, `list`, `ctl`, and error output still print while a
//!   Start Menu/Search launch allocates no console (bitty#2929).
//!
//! No raw `HANDLE` or pointer crosses the public API: handles are
//! `std::os::windows::io::OwnedHandle` values owned by the types above.
//!
//! On every other platform the crate compiles to an empty surface with no
//! dependencies. The Windows-only items above are code spans, not intra-doc
//! links, so `cargo doc` stays warning-free on hosts where `cfg(windows)`
//! removes them from the crate.

#![deny(unsafe_code)]

#[cfg(windows)]
#[allow(unsafe_code)]
mod ffi;

#[cfg(windows)]
mod conpty;

#[cfg(windows)]
mod console;

#[cfg(windows)]
mod job;

#[cfg(windows)]
pub use conpty::{ChildSpec, ConPtyChild, ConPtyMaster};
#[cfg(windows)]
pub use console::{ConsoleStreams, adopt_parent_console};
#[cfg(windows)]
pub use job::{
    CREATE_SUSPENDED_FLAG, JobMember, JobObject, process_is_running, resume_suspended_process,
    terminate_process,
};
