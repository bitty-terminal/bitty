//! Linux Landlock enforcement for [`crate::confine_command_to_execs`]
//! (CTX-1151, issue #1984).
//!
//! This is the crate's only `#[allow(unsafe_code)]` module (the crate root
//! denies it, following the `bitty-winjob` precedent). Every `unsafe` block
//! below carries a `SAFETY` rationale. The public surface of this module is
//! `pub(crate)`; safe callers go through the crate root.
//!
//! Only three raw syscalls are used (`SYS_landlock_*` via the workspace
//! `libc`, numbers `444`/`445`/`446` on every supported Linux target). File
//! opens stay in safe code (`O_PATH` via `OpenOptions::custom_flags`), so
//! no path pointer ever crosses an `unsafe` boundary.

use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;

use crate::{MAX_CONFINEMENT_RULES, MAX_PATH_BYTES, SandboxError, SandboxErrorKind};

// ── UAPI (linux/landlock.h, ABI v1, frozen since 5.13) ────────────────────

/// `LANDLOCK_ACCESS_FS_EXECUTE`: the only access class this slice handles.
///
/// Handling exactly one class keeps the tool's reads, writes, and memory
/// maps untouched (loader, libraries, repository I/O) while every exec
/// outside the rule set fails with `EACCES`.
const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1;

/// `LANDLOCK_RULE_PATH_BENEATH`: on a file fd grants that file itself.
const LANDLOCK_RULE_PATH_BENEATH: libc::c_long = 1;

/// `struct landlock_ruleset_attr` (ABI v1: handled access only).
#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

/// `struct landlock_path_beneath_attr` (ABI v1).
#[repr(C)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: libc::c_int,
}

// ── owned ruleset fd ──────────────────────────────────────────────────────

/// Owned Landlock ruleset fd: the parent-side half of the confinement.
///
/// Created and populated before the fork; the child's `pre_exec` hook
/// captures only the fd *number* (see [`apply`]). Never cloned, so the fd
/// closes exactly once, in [`Drop`].
#[derive(Debug)]
pub(crate) struct RulesetGuard {
    fd: libc::c_int,
}

impl RulesetGuard {
    fn new(fd: libc::c_int) -> Self {
        Self { fd }
    }

    /// Borrow the fd number for the `pre_exec` capture.
    ///
    /// This copies the number only; ownership (and the single close) stays
    /// with the guard, which must outlive `Command::spawn`.
    fn raw(&self) -> libc::c_int {
        self.fd
    }
}

impl Drop for RulesetGuard {
    fn drop(&mut self) {
        // SAFETY: the fd came from `landlock_create_ruleset` in
        // `create_ruleset`, is owned by this guard (never cloned, never
        // handed to an owned-handle type), and `Drop` runs once; a failed
        // close is ignored because there is nothing fail-closed left to do
        // (the spawn already returned or never ran).
        unsafe {
            libc::close(self.fd);
        }
    }
}

// ── raw syscalls ──────────────────────────────────────────────────────────

/// Create an `EXECUTE`-handling ruleset; doubles as the ABI probe.
fn create_ruleset() -> Result<RulesetGuard, io::Error> {
    let attr = RulesetAttr {
        handled_access_fs: LANDLOCK_ACCESS_FS_EXECUTE,
    };
    // SAFETY: `SYS_landlock_create_ruleset` with a valid pointer to a
    // stack `RulesetAttr`, its exact size, and no flags. The kernel only
    // reads the struct (never writes through it); on kernels without
    // Landlock the number is unimplemented and the call fails, which the
    // caller maps to `UnsupportedKernel`. No memory unsafety on any path.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            std::mem::size_of::<RulesetAttr>(),
            0,
        )
    };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(RulesetGuard::new(ret as libc::c_int))
    }
}

/// Grant `EXECUTE` on the file behind `file_fd` in `ruleset_fd`.
fn add_exec_rule(ruleset_fd: libc::c_int, file_fd: libc::c_int) -> Result<(), io::Error> {
    let attr = PathBeneathAttr {
        allowed_access: LANDLOCK_ACCESS_FS_EXECUTE,
        parent_fd: file_fd,
    };
    // SAFETY: `SYS_landlock_add_rule` with a valid pointer to a stack
    // `PathBeneathAttr` (read-only to the kernel), the two owned fds as
    // integers, and no flags. Both fds stay open for the call (the ruleset
    // via its guard, the file via its `File` owner in `apply`). No memory
    // unsafety on any path.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset_fd,
            LANDLOCK_RULE_PATH_BENEATH,
            &attr as *const PathBeneathAttr,
            0,
        )
    };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Confine the caller to the ruleset (child side, `pre_exec` only).
fn restrict_self(ruleset_fd: libc::c_int) -> Result<(), io::Error> {
    // SAFETY: `SYS_landlock_restrict_self` takes no pointers — only the
    // inherited ruleset fd number and no flags. The effect (confining the
    // calling process, irreversible) is exactly the intended fail-closed
    // enforcement, and it runs only in the post-fork child (see `apply`).
    // Failure aborts the spawn via the `pre_exec` error path.
    let ret = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset_fd, 0) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// ── safe orchestration ────────────────────────────────────────────────────

/// Whether this kernel supports the Landlock calls this slice needs.
///
/// Test-only seam: production goes through [`apply`], whose ruleset
/// creation fails closed the same way. A direct creation attempt *is* the
/// probe: it proves the numbers, the LSM, and the seccomp filter all
/// cooperate, not just a version sticker (the `VERSION` pseudo-probe fails
/// `EINVAL` on some 6.x kernels where creation succeeds).
#[cfg(test)]
pub(crate) fn probe_supported() -> Result<(), SandboxError> {
    create_ruleset().map(|_| ()).map_err(|error| {
        SandboxError::new(
            SandboxErrorKind::UnsupportedKernel,
            format!("landlock probe refused (need kernel 5.13+ with the Landlock LSM): {error}"),
        )
    })
}

/// Open one rule target `O_PATH` (safe; no read permission needed, the fd
/// only names the inode for `add_exec_rule`).
fn open_rule_file(path: &Path) -> io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
        .open(path)
}

// ── ELF interpreter discovery ─────────────────────────────────────────────
//
// A dynamically linked tool execs only if its `PT_INTERP` loader is also
// granted `EXECUTE` (the kernel checks the interpreter during `binfmt_elf`
// loading; shared libraries mapped afterwards need no grant). The loader
// path is read from the binary itself — never from configuration or the
// environment — by a bounded, pure parser below. Anything unparseable
// (scripts, exotic headers) simply contributes no interpreter rule, and the
// exec then refuses fail-closed if it needed one.

/// Maximum program headers parsed from one binary.
const MAX_PHNUM: usize = 1024;

/// Maximum bytes read for the program-header table plus interpreter.
const MAX_PARSE_BYTES: usize = 64 * 1024;

/// Extract the `PT_INTERP` path from an ELF binary, if it names one.
///
/// Bounded and fail-closed: any malformed, truncated, oversize, or
/// non-little-endian header yields `None` (no interpreter rule), and a
/// dynamically linked tool without its rule then refuses at exec. Returns
/// `None` for non-ELF files (scripts) and statically linked binaries.
pub(crate) fn elf_interp(path: &Path) -> Option<std::path::PathBuf> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let mut ident = [0u8; 64];
    file.read_exact(&mut ident[..64]).ok()?;
    if ident[0..4] != [0x7f, b'E', b'L', b'F'] {
        return None;
    }
    // Class: 1 = 32-bit, 2 = 64-bit. Data: 1 = little-endian (every
    // supported Linux target); anything else refuses the parse.
    let is_64 = match ident[4] {
        1 => false,
        2 => true,
        _ => return None,
    };
    if ident[5] != 1 {
        return None;
    }
    let (phoff, phentsize, phnum) = if is_64 {
        let phoff = u64::from_le_bytes(ident[32..40].try_into().ok()?) as u64;
        let phentsize = u16::from_le_bytes(ident[54..56].try_into().ok()?) as usize;
        let phnum = u16::from_le_bytes(ident[56..58].try_into().ok()?) as usize;
        if phentsize != 56 {
            return None;
        }
        (phoff, phentsize, phnum)
    } else {
        let phoff = u32::from_le_bytes(ident[28..32].try_into().ok()?) as u64;
        let phentsize = u16::from_le_bytes(ident[42..44].try_into().ok()?) as usize;
        let phnum = u16::from_le_bytes(ident[44..46].try_into().ok()?) as usize;
        if phentsize != 32 {
            return None;
        }
        (phoff, phentsize, phnum)
    };
    if phnum == 0 || phnum > MAX_PHNUM {
        return None;
    }
    let table_len = phnum.checked_mul(phentsize)?;
    if table_len > MAX_PARSE_BYTES {
        return None;
    }
    file.seek(SeekFrom::Start(phoff)).ok()?;
    let mut table = vec![0u8; table_len];
    file.read_exact(&mut table).ok()?;
    for header in table.chunks_exact(phentsize) {
        // `p_type` is the first 32-bit field on both classes.
        let p_type = u32::from_le_bytes(header[0..4].try_into().ok()?);
        if p_type != 3 {
            continue;
        }
        // 64-bit layout: type(4) flags(4) offset(8) vaddr(8) paddr(8)
        // filesz(8) memsz(8) align(8). 32-bit layout: type(4) offset(4)
        // vaddr(4) paddr(4) filesz(4) memsz(4) flags(4) align(4).
        let (offset, filesz) = if is_64 {
            let offset = u64::from_le_bytes(header[8..16].try_into().ok()?);
            let filesz = u64::from_le_bytes(header[32..40].try_into().ok()?);
            (offset, filesz)
        } else {
            let offset = u32::from_le_bytes(header[4..8].try_into().ok()?) as u64;
            let filesz = u32::from_le_bytes(header[16..20].try_into().ok()?) as u64;
            (offset, filesz)
        };
        if filesz == 0 || filesz > MAX_PATH_BYTES as u64 {
            return None;
        }
        if offset.saturating_add(filesz) > MAX_PARSE_BYTES as u64 {
            return None;
        }
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut bytes = vec![0u8; filesz as usize];
        file.read_exact(&mut bytes).ok()?;
        // `PT_INTERP` is NUL-terminated; a missing terminator refuses.
        let end = bytes.iter().position(|byte| *byte == 0)?;
        bytes.truncate(end);
        if bytes.is_empty() {
            return None;
        }
        use std::os::unix::ffi::OsStringExt;
        let interp = std::ffi::OsString::from_vec(bytes);
        let path = std::path::PathBuf::from(interp);
        if !path.is_absolute() {
            return None;
        }
        return Some(path);
    }
    None
}

/// Install the `EXECUTE` ruleset on `command` (parent side) and arm the
/// child-side `restrict_self` hook.
///
/// The caller must hold the returned [`crate::ExecConfinement`] guard until
/// `Command::spawn` returns: the guard owns the ruleset fd whose number the
/// `pre_exec` closure captured. Every error maps to [`SandboxError`] and
/// leaves no process behind — the caller must propagate instead of
/// spawning.
pub(crate) fn apply(
    command: &mut std::process::Command,
    exec_paths: &[std::path::PathBuf],
) -> Result<crate::ExecConfinement, SandboxError> {
    let ruleset = create_ruleset().map_err(|error| {
        SandboxError::new(
            SandboxErrorKind::UnsupportedKernel,
            format!(
                "landlock_create_ruleset refused (need kernel 5.13+ with the Landlock LSM): {error}"
            ),
        )
    })?;
    // Rule targets: each executable plus its ELF interpreter (the kernel
    // checks `EXECUTE` on `PT_INTERP` at load; libraries need no grant).
    // Deduplicated and bounded; every failure below refuses before any
    // fork, so no process ever starts unconfined.
    let mut targets: Vec<std::path::PathBuf> = Vec::new();
    for path in exec_paths {
        if !targets.contains(path) {
            targets.push(path.clone());
        }
        if let Some(interp) = elf_interp(path) {
            if !targets.contains(&interp) {
                targets.push(interp);
            }
        }
    }
    if targets.len() > MAX_CONFINEMENT_RULES {
        return Err(SandboxError::new(
            SandboxErrorKind::InvalidInput,
            "confinement rule set exceeds 32 entries",
        ));
    }
    for path in &targets {
        let file = open_rule_file(path).map_err(|error| {
            SandboxError::new(
                SandboxErrorKind::SetupFailed,
                format!("confinement rule open refused: {error}"),
            )
        })?;
        add_exec_rule(ruleset.raw(), file.as_raw_fd()).map_err(|error| {
            SandboxError::new(
                SandboxErrorKind::SetupFailed,
                format!("landlock_add_rule refused: {error}"),
            )
        })?;
        // `file` drops here: the rule persists in the ruleset, and only
        // the ruleset fd crosses the fork below.
    }
    let fd = ruleset.raw();
    // SAFETY (`pre_exec` registration): the closure runs once in the
    // post-fork, single-threaded child, after stdio setup and immediately
    // before `exec`. It performs exactly one raw syscall
    // (`restrict_self`) with the captured fd number — no allocation, no
    // locks, no std I/O, no access to parent memory beyond the copied
    // integer — then returns. `Ok` proceeds to an exec the ruleset
    // constrains; `Err` makes `Command::spawn` fail in the parent, so a
    // confinement failure can never silently run unconfined. The ruleset
    // fd itself is `CLOEXEC` (Landlock fds carry it by design), so it
    // never leaks into the exec'd program.
    unsafe {
        command.pre_exec(move || restrict_self(fd));
    }
    Ok(crate::ExecConfinement { ruleset })
}
