//! `Runtime` — Kitty local-medium file/shm reads (`t=f`/`t=t`/`t=s`, S3 of #1849).
//!
//! Open-time half of the local-medium sandbox. The parser (`bitty-vt`
//! `kitty_apc`, CTX-0950) already enforces the parse-time half — non-empty
//! names within [`bitty_vt::KITTY_APC_PATH_MAX_BYTES`], no NUL bytes, no
//! `..` components (file mediums), the
//! [`bitty_vt::KITTY_APC_TMP_NAME_MARKER`] substring (`t=t`), strict POSIX
//! shm shape (`t=s`) — and completes local mediums single-shot, so the
//! `payload` arriving here is the validated name bytes with `S=`/`O=` in
//! `control`. This module performs the open-time half where the object is
//! opened, following ghostty's `readFile`/`readSharedMemory` order
//! (open-before-validate, regular-file check on the open fd, read under the
//! decode caps).
//!
//! # Fail-closed order (every step refuses without storing)
//!
//! 1. Shared shape re-check (defense in depth; the parser already did this,
//!    but the opener never trusts the caller): empty, overlong, NUL,
//!    `..` traversal (file mediums), missing temp marker (`t=t`), malformed
//!    shm name (`t=s`).
//! 2. Pure `S=`/`O=` arithmetic with checked adds: overflow, `S=` above the
//!    byte cap, or an offset past the byte cap refuses before any I/O.
//! 3. Platform gate (see below).
//! 4. `t=t` containment pre-check on the canonicalized path string: the
//!    resolved path must sit under a temp-dir allowlist root *and* still
//!    carry the marker (a symlink final component resolves away from the
//!    marker name and is refused here; the `O_NOFOLLOW` open would refuse
//!    it again as `ELOOP`).
//! 5. `O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC` open. `O_NOFOLLOW` refuses
//!    symlink final components (`ELOOP`); `O_NONBLOCK` keeps FIFO opens
//!    from parking the render thread until a writer arrives (the fstat next
//!    refuses the FIFO anyway).
//! 6. Linux only, `t=t`: re-derive the open file's canonical target from
//!    `/proc/self/fd` and re-check containment, closing the
//!    canonicalize-then-open rename race.
//! 7. `fstat` on the fd (never `stat` by path): non-regular files
//!    (directories, FIFOs, sockets, devices) are refused.
//! 8. `fstat` size precheck against the read cap: oversize files are refused
//!    before any large allocation.
//! 9. Bounded read: `S=`-exact bytes when `S != 0` (short from shrinkage or
//!    long from growth both fail closed), otherwise read-to-end capped at
//!    the byte cap plus one probe byte (growth past the cap fails closed).
//! 10. `t=t`/`t=s` unlink-after-read, best effort (unlink errors never fail
//!     admission: the bytes were already read).
//!
//! # Platform scope (decided for this slice)
//!
//! - Files (`t=f`/`t=t`): Unix only. Linux opens via `rustix`, macOS/BSD
//!   via `nix`, both with `O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC`. Windows is
//!   denied fail-closed: `std` offers no `O_NOFOLLOW` equivalent and the
//!   `windows-sys` reparse-point plumbing would be a new supply-chain and
//!   audit surface for one slice.
//! - Shared memory (`t=s`): Linux only (`/dev/shm/<name>`), denied
//!   elsewhere. macOS `shm_open` objects live outside any stable
//!   caller-visible filesystem path, so the same containment story cannot
//!   be told; Windows has no POSIX shm namespace at all.
//! - The `/proc/self/fd` re-containment (step 6) is Linux only. Other Unix
//!   keeps the pre-open canonicalize check plus `O_NOFOLLOW` (documented
//!   residual: a privileged same-user rename race between canonicalize and
//!   open could swap a regular file; the swapped bytes still decode under
//!   the caps and charge to the same origin quota).
//!
//! # Temp-dir allowlist (`t=t`)
//!
//! The canonicalized wire path must start with one of (each canonicalized
//! fail-closed, missing entries skipped): [`std::env::temp_dir`] (honors
//! `TMPDIR`, read from the terminal process environment, never from PTY
//! bytes), `/tmp`, `/var/tmp`, `/dev/shm`. macOS temp dirs
//! (`/var/folders/...`, with `/tmp` a symlink to `/private/tmp`) resolve
//! through canonicalization on both sides before comparison.
//!
//! # `S=`/`O=` semantics
//!
//! `O=` is the start offset, `S=` the exact byte count (`0` reads to end).
//! The read cap is `min(declared, 64 MiB)`, where `declared` is the exact
//! raw `s * v * channels` size when a non-zero raw claim is present and
//! 64 MiB otherwise (PNG `IHDR` governs after the read, like direct
//! streams). Offsets past the byte cap can never yield admittable data and
//! are refused.
//!
//! # Quota and query wiring
//!
//! Read bytes flow into the existing transmit/display seams, so per-origin
//! quotas (S5, #1849) apply unchanged: the stored image counts against the
//! draining stream's origin, and quota-full refuses without storing.
//! File-backed `a=q` probes test-load the *read* bytes (never the name
//! bytes); read failures answer `EINVAL:bad data` as silent protocol
//! replies honoring `q=` (S2), never stderr.
//!
//! # Known limitation: `o=z` on local mediums
//!
//! The parser consumes `o=z` (zlib) only for direct streams (the payload is
//! decompressed during reassembly); the flag is not carried on completed
//! local mediums, so stored file/shm bytes decode as-is. Zlib-wrapped
//! stored bytes therefore fail closed at decode with nothing stored. No
//! parser change is made for this slice; plumbing the flag through
//! `KittyCompleted`/`TerminalAction` is follow-up work if interop needs it.
//!
//! # Relative paths
//!
//! A `..`-free relative `t=f` name passes parse-time checks and resolves
//! against the terminal process working directory (inherited from the
//! launcher, never from PTY bytes), matching kitty's server-side
//! resolution. Absolute temp-dir fixtures are preferred; tests use them.
//!
//! Time O(read cap) worst case on the bounded read; space O(bytes read),
//! always within the 64 MiB decode ceiling.

#![forbid(unsafe_code)]

use super::*;
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
use std::io::{Read, Seek, SeekFrom};
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
use std::path::{Path, PathBuf};

use bitty_rich::{
    KITTY_DECODE_MAX_BYTES, KITTY_DECODE_MAX_DIMENSION, KITTY_DECODE_MAX_PIXELS, KITTY_FORMAT_RGB,
    KITTY_FORMAT_RGBA,
};
use bitty_vt::{KITTY_APC_PATH_MAX_BYTES, KITTY_APC_SHM_NAME_MAX, KITTY_APC_TMP_NAME_MARKER};

/// Byte cap for one local-medium read: the Core decode ceiling
/// ([`KITTY_DECODE_MAX_BYTES`]), mirroring the parser's default decode cap
/// so the opener can never hand the decoder more than it admits.
const KITTY_LOCAL_READ_CAP: usize = KITTY_DECODE_MAX_BYTES;

/// Async worker bounds (CTX-1110, follow-up of S3 #1918 CodeRabbit Major).
///
/// The PTY poll never blocks on filesystem I/O: local-medium reads run on
/// short-lived worker threads (one in flight at a time, FIFO) while the
/// poll keeps draining every pane. Thread caps count in-flight + timed-out
/// orphans (hung threads still blocked in the kernel), so a hung filesystem
/// leaks at most a bounded number of threads and then refuses fail-closed;
/// queue caps count admitted-but-unstarted requests (each holds only a small
/// `KittyLocalRequest`, no thread), so a burst of frames/images from one
/// pane is admitted instead of silently dropped.
pub(crate) const KITTY_LOCAL_MAX_PENDING_GLOBAL: usize = 8;
/// Per-origin in-flight + orphan thread cap: one origin can hold at most
/// two worker threads, so a noisy pane cannot starve other origins out of
/// the global eight.
pub(crate) const KITTY_LOCAL_MAX_PENDING_PER_ORIGIN: usize = 2;
/// Per-origin queued-request cap (CTX-1110 fix pass, CodeRabbit Major #1):
/// a 10-frame `a=f,t=f` burst from one pane must not drop 8 frames. Queued
/// requests hold no thread, so 32 per origin is cheap (each is one small
/// request plus a path-sized name) while still bounding memory.
pub(crate) const KITTY_LOCAL_MAX_QUEUED_PER_ORIGIN: usize = 32;
/// Global queued-request cap (CTX-1110 fix pass): two full per-origin
/// bursts plus headroom; with the 8 thread slots above the total outstanding
/// work stays bounded at 72 slots worst case.
pub(crate) const KITTY_LOCAL_MAX_QUEUED_GLOBAL: usize = 64;
/// Generous wall-clock bound for one local-medium read (CTX-1110).
///
/// Rationale: local SSD reads of at most 64 MiB complete in milliseconds;
/// spinning disks take hundreds of milliseconds and a loaded network
/// filesystem can take around a second. Three seconds covers legitimate
/// slow disks without false timeouts (a too-small bound such as 100 ms
/// would break them), while a hung mount still degrades to a silent
/// refusal after at most this long. The poll itself never waits: the
/// deadline is checked non-blockingly on later pumps.
pub(crate) const KITTY_LOCAL_READ_TIMEOUT_SECS: u64 = 3;

// Per-`Runtime` test hooks (CTX-1110, unit tests only; production leaves
// them at zero/false). Stored on the `Runtime` — never process-global —
// so parallel tests cannot interfere: each test owns its `Runtime`.
// `kitty_local_test_timeout_ms` is the tiny deadline override for hermetic
// fast timeout proofs (`0` = the generous production default),
// `kitty_local_test_delay_ms` the injected worker sleep simulating a hung
// filesystem (`0` = none), and `kitty_local_test_panic` the injected worker
// panic proving panic isolation. Declared on `Runtime` in `runtime.rs`
// next to the queue fields; unit tests (child module below) set them.

/// Typed local-medium read rejection (fail closed, stores nothing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KittyLocalReadError {
    /// Name shape refused: empty, overlong, NUL-bearing, `..` traversal,
    /// missing `t=t` marker, malformed shm name, temp-dir containment miss,
    /// or a direct medium misrouted here.
    BadName,
    /// Platform without a sandboxed open for this medium (non-Unix files,
    /// non-Linux shm). Fail closed, never attempted.
    UnsupportedPlatform,
    /// The opened fd is not a regular file (directory, FIFO, socket,
    /// device).
    NotRegularFile,
    /// `S=`/`O=` checked arithmetic overflowed, `S=` exceeds the byte cap,
    /// the `fstat` size exceeds the read cap (refused pre-read), or the
    /// capped read observed more bytes than the cap (TOCTOU growth).
    TooLarge,
    /// `O=` past end-of-file, or an `S=`-exact read that came up short
    /// (TOCTOU shrinkage) or long.
    OffsetOutOfRange,
    /// Filesystem I/O refused the operation. Carries only the operation
    /// tag (`open`, `stat`, `seek`, `read`, `canonicalize`, `fd-target`),
    /// never path bytes (which may name sensitive locations).
    Io(&'static str),
}

impl std::fmt::Display for KittyLocalReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadName => write!(f, "kitty local path rejected by sandbox"),
            Self::UnsupportedPlatform => {
                write!(f, "kitty local medium not supported on this platform")
            }
            Self::NotRegularFile => write!(f, "kitty local object is not a regular file"),
            Self::TooLarge => write!(f, "kitty local read exceeds the byte cap"),
            Self::OffsetOutOfRange => write!(f, "kitty local offset past end of object"),
            Self::Io(op) => write!(f, "kitty local object I/O refused ({op})"),
        }
    }
}

impl std::error::Error for KittyLocalReadError {}

/// Whether `name` carries the `t=t` marker substring (parser parity).
fn contains_tmp_marker(name: &[u8]) -> bool {
    name.windows(KITTY_APC_TMP_NAME_MARKER.len())
        .any(|w| w == KITTY_APC_TMP_NAME_MARKER.as_bytes())
}

/// Shared name-shape re-check (parser parity, defense in depth: the opener
/// never trusts the caller).
fn check_name_shape(medium: bitty_vt::KittyMedium, name: &[u8]) -> Result<(), KittyLocalReadError> {
    if name.is_empty() || name.len() > KITTY_APC_PATH_MAX_BYTES || name.contains(&0) {
        return Err(KittyLocalReadError::BadName);
    }
    match medium {
        bitty_vt::KittyMedium::Direct => Err(KittyLocalReadError::BadName),
        bitty_vt::KittyMedium::SharedMemory => {
            let valid = name.len() >= 2
                && name.len() <= KITTY_APC_SHM_NAME_MAX
                && name[0] == b'/'
                && !name[1..].contains(&b'/');
            if valid {
                Ok(())
            } else {
                Err(KittyLocalReadError::BadName)
            }
        }
        bitty_vt::KittyMedium::File | bitty_vt::KittyMedium::TempFile => {
            if medium == bitty_vt::KittyMedium::TempFile && !contains_tmp_marker(name) {
                return Err(KittyLocalReadError::BadName);
            }
            if name.split(|&b| b == b'/').any(|c| c == b"..") {
                return Err(KittyLocalReadError::BadName);
            }
            Ok(())
        }
    }
}

/// Bounded read cap: `min(declared, [`KITTY_LOCAL_READ_CAP`]) with checked
/// arithmetic (`S=` exact size when non-zero, else the raw claim or the
/// byte cap for PNG/undeclared).
fn read_cap_bytes(
    format_f: u32,
    width_s: Option<u32>,
    height_v: Option<u32>,
    data_size: u32,
) -> Result<usize, KittyLocalReadError> {
    let declared: usize = match (format_f, width_s, height_v) {
        (f, Some(w), Some(h))
            if (f == KITTY_FORMAT_RGB || f == KITTY_FORMAT_RGBA) && w != 0 && h != 0 =>
        {
            if w > KITTY_DECODE_MAX_DIMENSION || h > KITTY_DECODE_MAX_DIMENSION {
                return Err(KittyLocalReadError::TooLarge);
            }
            let pixels = u64::from(w) * u64::from(h);
            if pixels > KITTY_DECODE_MAX_PIXELS {
                return Err(KittyLocalReadError::TooLarge);
            }
            let channels: u64 = if f == KITTY_FORMAT_RGB { 3 } else { 4 };
            let bytes = pixels
                .checked_mul(channels)
                .ok_or(KittyLocalReadError::TooLarge)?;
            usize::try_from(bytes)
                .map_err(|_| KittyLocalReadError::TooLarge)?
                .min(KITTY_LOCAL_READ_CAP)
        }
        _ => KITTY_LOCAL_READ_CAP,
    };
    if data_size != 0 {
        let want = usize::try_from(data_size).map_err(|_| KittyLocalReadError::TooLarge)?;
        if want > KITTY_LOCAL_READ_CAP {
            return Err(KittyLocalReadError::TooLarge);
        }
        Ok(want.min(declared))
    } else {
        Ok(declared)
    }
}

/// Reads one local-medium object into memory, bounded and fail-closed.
///
/// `name` is the wire name bytes (validated path for `t=f`/`t=t`, POSIX shm
/// name for `t=s`); `data_size`/`data_offset` are the wire `S=`/`O=`
/// values; `format_f`/`width_s`/`height_v` bound the read like the parser's
/// raw-claim gate. On success for `t=t`/`t=s` the object is unlinked after
/// reading (best effort). Failures store nothing.
pub fn read_kitty_local(
    medium: bitty_vt::KittyMedium,
    name: &[u8],
    data_size: u32,
    data_offset: u32,
    format_f: u32,
    width_s: Option<u32>,
    height_v: Option<u32>,
) -> Result<Vec<u8>, KittyLocalReadError> {
    check_name_shape(medium, name)?;
    let cap = read_cap_bytes(format_f, width_s, height_v, data_size)?;
    // Checked `O=`/`S=` arithmetic before any I/O: overflow refuses here,
    // and offsets past the byte cap can never yield admittable data.
    let offset = u64::from(data_offset);
    if offset > KITTY_LOCAL_READ_CAP as u64 {
        return Err(KittyLocalReadError::OffsetOutOfRange);
    }
    if data_size != 0 {
        offset
            .checked_add(u64::from(data_size))
            .ok_or(KittyLocalReadError::TooLarge)?;
    }
    #[cfg(not(unix))]
    {
        let _ = cap;
        Err(KittyLocalReadError::UnsupportedPlatform)
    }
    #[cfg(unix)]
    {
        read_kitty_local_unix(medium, name, data_size, offset, cap)
    }
}

/// Candidate temp-dir roots for `t=t` containment, each canonicalized
/// fail-closed (missing entries skipped, duplicates dropped).
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
fn temp_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for candidate in [
        std::env::temp_dir(),
        PathBuf::from("/tmp"),
        PathBuf::from("/var/tmp"),
        PathBuf::from("/dev/shm"),
    ] {
        match candidate.canonicalize() {
            Ok(canon) if canon.is_absolute() && !roots.contains(&canon) => roots.push(canon),
            _ => {}
        }
    }
    roots
}

/// `t=t` containment on the path string: the canonicalized target must sit
/// under a temp-dir root and still carry the marker (a symlink final
/// component resolving to a differently-named file is refused here; the
/// `O_NOFOLLOW` open refuses it again as `ELOOP`).
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
fn check_temp_containment(path: &Path) -> Result<(), KittyLocalReadError> {
    use std::os::unix::ffi::OsStrExt;
    let canonical = path
        .canonicalize()
        .map_err(|_| KittyLocalReadError::Io("canonicalize"))?;
    if !contains_tmp_marker(canonical.as_os_str().as_bytes()) {
        return Err(KittyLocalReadError::BadName);
    }
    if temp_roots().iter().any(|root| canonical.starts_with(root)) {
        Ok(())
    } else {
        Err(KittyLocalReadError::BadName)
    }
}

/// Linux-only re-containment on the open fd: re-derives the canonical target
/// from `/proc/self/fd` and re-checks the temp-dir roots, closing the
/// canonicalize-then-open rename race. Refuses fail-closed when the target
/// cannot be derived.
#[cfg(target_os = "linux")]
fn check_fd_containment(file: &std::fs::File) -> Result<(), KittyLocalReadError> {
    use std::os::fd::{AsFd, AsRawFd};
    let fd = file.as_fd().as_raw_fd();
    let target = std::fs::read_link(format!("/proc/self/fd/{fd}"))
        .map_err(|_| KittyLocalReadError::Io("fd-target"))?;
    if temp_roots().iter().any(|root| target.starts_with(root)) {
        Ok(())
    } else {
        Err(KittyLocalReadError::BadName)
    }
}

/// `O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC` open on Linux (via `rustix`).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn open_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(std::fs::File::from(fd))
}

/// `O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC` open on macOS/BSD (via `nix`).
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
fn open_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    use nix::fcntl::OFlag;
    use nix::sys::stat::Mode;
    let fd = nix::fcntl::open(
        path,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    Ok(std::fs::File::from(fd))
}

/// Unix open-time half: containment, `O_NOFOLLOW` open, fd `fstat`, size
/// precheck, bounded read, unlink-after-read for `t=t`/`t=s`.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
fn read_kitty_local_unix(
    medium: bitty_vt::KittyMedium,
    name: &[u8],
    data_size: u32,
    offset: u64,
    cap: usize,
) -> Result<Vec<u8>, KittyLocalReadError> {
    use std::os::unix::ffi::OsStrExt;

    let path: PathBuf = match medium {
        bitty_vt::KittyMedium::SharedMemory => {
            #[cfg(not(target_os = "linux"))]
            {
                return Err(KittyLocalReadError::UnsupportedPlatform);
            }
            #[cfg(target_os = "linux")]
            {
                let stripped = name
                    .strip_prefix(b"/")
                    .ok_or(KittyLocalReadError::BadName)?;
                let mut shm = PathBuf::from("/dev/shm");
                shm.push(std::ffi::OsStr::from_bytes(stripped));
                shm
            }
        }
        bitty_vt::KittyMedium::File | bitty_vt::KittyMedium::TempFile => {
            PathBuf::from(std::ffi::OsStr::from_bytes(name))
        }
        bitty_vt::KittyMedium::Direct => return Err(KittyLocalReadError::BadName),
    };

    if medium == bitty_vt::KittyMedium::TempFile {
        check_temp_containment(&path)?;
    }
    let mut file = open_nofollow(&path).map_err(|_| KittyLocalReadError::Io("open"))?;
    #[cfg(target_os = "linux")]
    if medium == bitty_vt::KittyMedium::TempFile {
        check_fd_containment(&file)?;
    }
    // `fstat` on the fd (never `stat` by path): the type and size describe
    // the opened object, not whatever the path names now.
    let meta = file
        .metadata()
        .map_err(|_| KittyLocalReadError::Io("stat"))?;
    if !meta.is_file() {
        return Err(KittyLocalReadError::NotRegularFile);
    }
    let len = meta.len();
    file.seek(SeekFrom::Start(offset))
        .map_err(|_| KittyLocalReadError::Io("seek"))?;

    let mut bytes: Vec<u8>;
    if data_size != 0 {
        let want = usize::try_from(data_size).map_err(|_| KittyLocalReadError::TooLarge)?;
        let end = offset
            .checked_add(u64::from(data_size))
            .ok_or(KittyLocalReadError::TooLarge)?;
        if len < end {
            return Err(KittyLocalReadError::OffsetOutOfRange);
        }
        bytes = Vec::with_capacity(want);
        // `+1` probe byte: growth past the exact claim fails closed instead
        // of truncating silently.
        file.take(u64::from(data_size).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| KittyLocalReadError::Io("read"))?;
        if bytes.len() != want {
            return Err(KittyLocalReadError::OffsetOutOfRange);
        }
    } else {
        if offset > len {
            return Err(KittyLocalReadError::OffsetOutOfRange);
        }
        if len - offset > cap as u64 {
            // Oversize refused pre-read: no large allocation is attempted.
            return Err(KittyLocalReadError::TooLarge);
        }
        bytes = Vec::new();
        // `+1` probe byte: TOCTOU growth past the cap fails closed.
        file.take((cap as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| KittyLocalReadError::Io("read"))?;
        if bytes.len() > cap {
            return Err(KittyLocalReadError::TooLarge);
        }
    }

    // Delete-after-read (`t=t`) / unlink-after-read (`t=s`), best effort:
    // an unlink failure never fails admission (the bytes were read), and a
    // symlink swap here can only remove the link itself, never its target.
    if medium == bitty_vt::KittyMedium::TempFile || medium == bitty_vt::KittyMedium::SharedMemory {
        let _ = std::fs::remove_file(&path);
    }
    Ok(bytes)
}

/// Other Unix targets without a sandboxed `O_NOFOLLOW` open (illumos,
/// Solaris, AIX, ...): fail closed without attempting I/O. `read_kitty_local`
/// still dispatches here under `cfg(unix)`, so the stub keeps the call site
/// compiling while denying the read as `UnsupportedPlatform`.
#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))
))]
fn read_kitty_local_unix(
    _medium: bitty_vt::KittyMedium,
    _name: &[u8],
    _data_size: u32,
    _offset: u64,
    _cap: usize,
) -> Result<Vec<u8>, KittyLocalReadError> {
    Err(KittyLocalReadError::UnsupportedPlatform)
}

/// Async local-medium operation (CTX-1110): what to do with the read bytes
/// once the worker finishes. All wire parameters are captured at enqueue
/// time (arrival order); the read bytes arrive later via the worker.
#[derive(Debug, Clone)]
pub(crate) enum KittyLocalOp {
    /// Transmit-and-display (or transmit-only) image.
    Display {
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        action_a: Option<char>,
        cols_c: u16,
        rows_r: u16,
        cursor_movement_c: u8,
        wire_image: u32,
        wire_placement: u32,
    },
    /// Combined `a=T,U=1` virtual prototype registration.
    Virtual {
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        cols_c: u16,
        rows_r: u16,
        wire_image: u32,
        wire_placement: u32,
        has_parent: bool,
    },
    /// File-backed `a=q` probe (test-loads, never stores).
    Query {
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        control: bitty_vt::KittyControlKeys,
    },
    /// Animation frame append (`a=f`, `r=0`) via a local medium.
    Frame {
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        control: bitty_vt::KittyControlKeys,
        action: Box<bitty_term_state::TerminalAction>,
    },
}

/// One queued local-medium read (CTX-1110): the sandbox read parameters
/// plus the deferred operation. The worker thread owns a clone of the
/// read parameters; the queue keeps this copy for completion.
#[derive(Debug, Clone)]
pub(crate) struct KittyLocalRequest {
    origin: Option<u64>,
    /// Delete generation stamped at enqueue (CTX-1110 fix pass): when the
    /// origin's generation moved before this request completes, a
    /// delete/clear/retire landed in between and the storing completion
    /// drops as stale (delete-wins) instead of resurrecting the image.
    delete_epoch: u64,
    /// Enqueue-time image anchor (CTX-1110 fix pass, CodeRabbit Major #3):
    /// cursor cell plus scrollback base on the origin's grid at drain time
    /// (the pane grid is already swapped in by `handle_pane_bytes`, so
    /// `self.state` is the emitting grid here). Completion places the blit
    /// at this cell and advances the cursor from it — reproducing the sync
    /// order (place at APC cursor, then advance past the image) even when
    /// prompt/output text drained after the enqueue moved the live cursor.
    anchor_col: u16,
    anchor_row: u16,
    scrollback_base: usize,
    op: KittyLocalOp,
    name: Box<[u8]>,
    medium: bitty_vt::KittyMedium,
    data_size: u32,
    data_offset: u32,
    format_f: u32,
    width_s: Option<u32>,
    height_v: Option<u32>,
}

/// One in-flight worker thread (CTX-1110): the request plus its deadline
/// and join handle. The handle is only joined after `is_finished` reports
/// ready, so the poll never blocks; panic payloads arrive via `join` and
/// degrade to refusals.
pub(crate) struct KittyLocalInFlight {
    request: KittyLocalRequest,
    deadline: std::time::Instant,
    handle: std::thread::JoinHandle<Result<Vec<u8>, KittyLocalReadError>>,
}

/// One timed-out orphan thread (CTX-1110): the worker is still blocked in
/// the kernel after its deadline passed. Kept only to bound the leak and
/// reclaim the handle once it finishes; dropped (detached) otherwise.
pub(crate) struct KittyLocalOrphan {
    origin: Option<u64>,
    handle: std::thread::JoinHandle<Result<Vec<u8>, KittyLocalReadError>>,
}

/// Design choice (CTX-1110, conservative new failure-mode surface).
///
/// Chosen: per-request short-lived worker threads, at most one in flight,
/// FIFO queue — over a persistent 1-2-thread pool or a synchronous read
/// with a wall-clock deadline.
///
/// - A synchronous deadline cannot work: regular-file `read` ignores
///   `O_NONBLOCK` for data and a hung network filesystem blocks
///   uninterruptibly in the kernel, so no same-thread primitive bounds it.
/// - A persistent pool needs restart logic on panic and still head-of-line
///   blocks a hung read behind one thread; per-request threads isolate
///   panic and timeout per read via `join` + deadline.
/// - One in flight (not two) minimizes thread, ordering, and leak surface:
///   global FIFO implies per-image FIFO, so frame N+1 cannot complete
///   before frame N of the same image and no per-key scheduler is needed.
///   The poll itself never waits, so every pane keeps draining while a
///   hung read pends; the only head-of-line is queued completions waiting
///   up to the generous timeout, bounded by the caps below.
impl Runtime {
    /// Pending async local-medium slots (CTX-1110, headless-observable
    /// test seam): queued + in-flight + timed-out orphans. `0` means no
    /// local-medium work is outstanding.
    #[must_use]
    pub fn kitty_local_pending_count(&self) -> usize {
        self.kitty_local_pending_total()
    }

    /// Total pending slots (queued + in-flight + orphans).
    pub(crate) fn kitty_local_pending_total(&self) -> usize {
        self.kitty_local_queue.len()
            + usize::from(self.kitty_local_in_flight.is_some())
            + self.kitty_local_orphans.len()
    }

    /// Queued (admitted, unstarted) requests globally. No thread held.
    pub(crate) fn kitty_local_queued_total(&self) -> usize {
        self.kitty_local_queue.len()
    }

    /// Queued requests held by one origin.
    pub(crate) fn kitty_local_queued_for_origin(&self, origin: Option<u64>) -> usize {
        self.kitty_local_queue
            .iter()
            .filter(|req| req.origin == origin)
            .count()
    }

    /// Live worker threads globally (in-flight plus timed-out orphans).
    pub(crate) fn kitty_local_threads_total(&self) -> usize {
        usize::from(self.kitty_local_in_flight.is_some()) + self.kitty_local_orphans.len()
    }

    /// Live worker threads held by one origin.
    pub(crate) fn kitty_local_threads_for_origin(&self, origin: Option<u64>) -> usize {
        let mut count = 0;
        if self
            .kitty_local_in_flight
            .as_ref()
            .is_some_and(|flight| flight.request.origin == origin)
        {
            count += 1;
        }
        for orphan in &self.kitty_local_orphans {
            if orphan.origin == origin {
                count += 1;
            }
        }
        count
    }

    /// Caps check before any I/O (CTX-1110; split CTX-1110 fix pass,
    /// CodeRabbit Major #1): thread caps (global plus per-origin
    /// in-flight/orphan limits, bounding kernel-blocked threads) plus the
    /// larger bounded queue caps (per-origin and global queued limits, no
    /// thread held). Refusals happen here, before spawning — and before any
    /// filesystem I/O, so a refused `t=t`/`t=s` object is never unlinked,
    /// exactly like a sync S3 read refusal (the unlink runs only after a
    /// successful bounded read; see `read_kitty_local_unix`). No leak
    /// regression: queue-full parity with the sync refusal posture.
    pub(crate) fn kitty_local_caps_allow(&self, origin: Option<u64>) -> bool {
        self.kitty_local_threads_total() < KITTY_LOCAL_MAX_PENDING_GLOBAL
            && self.kitty_local_threads_for_origin(origin) < KITTY_LOCAL_MAX_PENDING_PER_ORIGIN
            && self.kitty_local_queued_for_origin(origin) < KITTY_LOCAL_MAX_QUEUED_PER_ORIGIN
            && self.kitty_local_queued_total() < KITTY_LOCAL_MAX_QUEUED_GLOBAL
    }

    /// Earliest pending local-medium deadline, if any (CTX-1110 fix pass,
    /// CodeRabbit Major #2): the in-flight worker's wall-clock bound. The
    /// app event loop arms a timed wake at this instant so a completed or
    /// expired read applies promptly instead of sitting until the next
    /// incidental event under `ControlFlow::Wait`. Queued requests carry no
    /// deadline until promoted; orphans already expired.
    #[must_use]
    pub fn kitty_local_deadline(&self) -> Option<std::time::Instant> {
        self.kitty_local_in_flight
            .as_ref()
            .map(|flight| flight.deadline)
    }

    /// Current delete generation for one origin (CTX-1110 fix pass):
    /// missing entries are generation zero. Stamped on every queued
    /// request at enqueue; see [`Self::bump_kitty_local_delete_epoch`].
    pub(crate) fn kitty_local_delete_epoch_for(&self, origin: Option<u64>) -> u64 {
        self.kitty_local_delete_epoch
            .get(&origin)
            .copied()
            .unwrap_or(0)
    }

    /// Advances one origin's delete generation (CTX-1110 fix pass).
    ///
    /// Every rendered-layer delete/clear/retire for the origin calls this,
    /// so storing completions stamped earlier drop as stale (delete-wins).
    /// Over-invalidation across wire ids of the same origin is fail-closed
    /// (stores nothing) by design: precision per wire id would need
    /// unbounded per-id state for no visible gain.
    pub(crate) fn bump_kitty_local_delete_epoch(&mut self, origin: Option<u64>) {
        let next = self.kitty_local_delete_epoch_for(origin).wrapping_add(1);
        self.kitty_local_delete_epoch.insert(origin, next);
    }

    /// Enqueues one display read without doing I/O (CTX-1110).
    ///
    /// Captures the draining origin (`self.kitty_origin`, already tagged by
    /// `handle_pane_bytes` for panes). Returns `false` when the bounded
    /// queue is full: the caller logs rate-limited and stores nothing.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn enqueue_kitty_local_display(
        &mut self,
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        action_a: Option<char>,
        cols_c: u16,
        rows_r: u16,
        cursor_movement_c: u8,
        name: &[u8],
        control: bitty_vt::KittyControlKeys,
    ) -> bool {
        let cursor = self.state.cursor().position;
        let request = KittyLocalRequest {
            origin: self.kitty_origin,
            delete_epoch: self.kitty_local_delete_epoch_for(self.kitty_origin),
            anchor_col: cursor.col,
            anchor_row: cursor.row,
            scrollback_base: self.state.scrollback_len(),
            op: KittyLocalOp::Display {
                format_f,
                width_s,
                height_v,
                action_a,
                cols_c,
                rows_r,
                cursor_movement_c,
                wire_image: control.image_id,
                wire_placement: control.placement_id,
            },
            name: name.to_vec().into_boxed_slice(),
            medium: control.medium,
            data_size: control.data_size,
            data_offset: control.data_offset,
            format_f,
            width_s,
            height_v,
        };
        self.enqueue_kitty_local(request)
    }

    /// Enqueues one virtual-prototype read without doing I/O (CTX-1110).
    /// Returns `false` when the bounded queue is full (caller logs).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn enqueue_kitty_local_virtual(
        &mut self,
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        cols_c: u16,
        rows_r: u16,
        name: &[u8],
        control: bitty_vt::KittyControlKeys,
    ) -> bool {
        let cursor = self.state.cursor().position;
        let request = KittyLocalRequest {
            origin: self.kitty_origin,
            delete_epoch: self.kitty_local_delete_epoch_for(self.kitty_origin),
            anchor_col: cursor.col,
            anchor_row: cursor.row,
            scrollback_base: self.state.scrollback_len(),
            op: KittyLocalOp::Virtual {
                format_f,
                width_s,
                height_v,
                cols_c,
                rows_r,
                wire_image: control.image_id,
                wire_placement: control.placement_id,
                has_parent: control.has_parent(),
            },
            name: name.to_vec().into_boxed_slice(),
            medium: control.medium,
            data_size: control.data_size,
            data_offset: control.data_offset,
            format_f,
            width_s,
            height_v,
        };
        self.enqueue_kitty_local(request)
    }

    /// Enqueues one file-backed query probe without doing I/O (CTX-1110).
    /// `I=` number lookups and silent queries (no `i=`/`I=`) answer
    /// synchronously without I/O like direct streams (delegated with an
    /// empty payload); only spec probes (`i=` non-zero) enqueue a worker
    /// read. Queue-full answers `EINVAL:bad data` immediately honoring
    /// `q=` (never stderr), like a refused read.
    pub(crate) fn enqueue_kitty_local_query(
        &mut self,
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        name: &[u8],
        control: bitty_vt::KittyControlKeys,
    ) {
        if control.image_id == 0 {
            self.answer_kitty_query(format_f, width_s, height_v, &[], control);
            return;
        }
        let cursor = self.state.cursor().position;
        let request = KittyLocalRequest {
            origin: self.kitty_origin,
            delete_epoch: self.kitty_local_delete_epoch_for(self.kitty_origin),
            anchor_col: cursor.col,
            anchor_row: cursor.row,
            scrollback_base: self.state.scrollback_len(),
            op: KittyLocalOp::Query {
                format_f,
                width_s,
                height_v,
                control,
            },
            name: name.to_vec().into_boxed_slice(),
            medium: control.medium,
            data_size: control.data_size,
            data_offset: control.data_offset,
            format_f,
            width_s,
            height_v,
        };
        if !self.enqueue_kitty_local(request) {
            if control.quiet >= 2 {
                return;
            }
            let reply = crate::queries::kitty_probe_einval_reply(control.image_id, "bad data");
            self.state.apply(&bitty_term_state::TerminalAction::Reply {
                bytes: reply.into_boxed_slice(),
            });
        }
    }

    /// Enqueues one local-medium animation frame without doing I/O
    /// (CTX-1110, `r=0` only; `r>0` gap edits never enqueue). Returns
    /// `false` when the bounded queue is full (caller logs).
    pub(crate) fn enqueue_kitty_local_frame(
        &mut self,
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        name: &[u8],
        control: bitty_vt::KittyControlKeys,
        action: &bitty_term_state::TerminalAction,
    ) -> bool {
        let cursor = self.state.cursor().position;
        let request = KittyLocalRequest {
            origin: self.kitty_origin,
            delete_epoch: self.kitty_local_delete_epoch_for(self.kitty_origin),
            anchor_col: cursor.col,
            anchor_row: cursor.row,
            scrollback_base: self.state.scrollback_len(),
            op: KittyLocalOp::Frame {
                format_f,
                width_s,
                height_v,
                control,
                action: Box::new(action.clone()),
            },
            name: name.to_vec().into_boxed_slice(),
            medium: control.medium,
            data_size: control.data_size,
            data_offset: control.data_offset,
            format_f,
            width_s,
            height_v,
        };
        self.enqueue_kitty_local(request)
    }

    /// Enqueues one local-medium read without doing I/O (CTX-1110).
    ///
    /// Returns `false` when the bounded queue is full (global or
    /// per-origin): the caller must fail closed immediately (display/frame
    /// rate-limited log, query `EINVAL` honoring `q=`), exactly like a
    /// refused read. On `true` the read is queued and promotion starts it
    /// promptly when no worker is in flight.
    pub(crate) fn enqueue_kitty_local(&mut self, request: KittyLocalRequest) -> bool {
        if !self.kitty_local_caps_allow(request.origin) {
            return false;
        }
        self.kitty_local_queue.push_back(request);
        self.promote_kitty_local_queue();
        true
    }

    /// Starts the queue head when no worker is in flight (CTX-1110,
    /// single-flight FIFO). Spawns at most one thread; spawn failure
    /// degrades to an immediate refusal of the head (popped, failed
    /// closed) so the queue cannot wedge on OS thread exhaustion.
    fn promote_kitty_local_queue(&mut self) {
        if self.kitty_local_in_flight.is_some() {
            return;
        }
        let Some(request) = self.kitty_local_queue.pop_front() else {
            return;
        };
        let name_owned: Vec<u8> = request.name.to_vec();
        let medium = request.medium;
        let data_size = request.data_size;
        let data_offset = request.data_offset;
        let format_f = request.format_f;
        let width_s = request.width_s;
        let height_v = request.height_v;
        // Per-Runtime test hooks are captured by value at spawn so parallel
        // tests (each owning its `Runtime`) cannot interfere.
        let test_delay_ms = self.kitty_local_test_delay_ms;
        let test_panic = self.kitty_local_test_panic;
        let timeout_ms = self.kitty_local_test_timeout_ms;
        let deadline = std::time::Instant::now()
            + if timeout_ms != 0 {
                std::time::Duration::from_millis(timeout_ms)
            } else {
                std::time::Duration::from_secs(KITTY_LOCAL_READ_TIMEOUT_SECS)
            };
        // The worker moves the whole `read_kitty_local` call (S3 sandbox
        // behavior preserved exactly: `O_NOFOLLOW`, containment, caps,
        // deny platforms) — never a reimplementation.
        // CTX-1110 fix pass (CodeRabbit Major #2): the worker wakes the
        // event loop on EVERY exit. The `PtyWaker` clone moves into the
        // thread behind a `Drop` guard, so normal completion, caught
        // panics, AND pre-`catch_unwind` early exits (the old `test_panic`
        // path panicked before the catch) all wake a `ControlFlow::Wait`
        // loop; without this a completed read sits unapplied until the
        // next incidental event. The guard runs during unwinding too, so
        // no exit path can skip the wake. Wakers are idempotent by
        // contract (see `PtyWaker` docs), so a racing second wake is safe.
        let waker: Option<crate::runtime::PtyWaker> = self.pty_waker.clone();
        let spawn = std::thread::Builder::new()
            .name("bitty-kitty-local".to_string())
            .spawn(move || {
                struct WakeOnDrop(Option<crate::runtime::PtyWaker>);
                impl Drop for WakeOnDrop {
                    fn drop(&mut self) {
                        if let Some(wake) = self.0.take() {
                            wake();
                        }
                    }
                }
                let _guard = WakeOnDrop(waker);
                // Panic isolation: a worker panic must not take down the
                // PTY poll. The whole operation — including the test hooks
                // — runs inside `catch_unwind`, which maps a panic to a
                // refusal here (plus `join` below maps a join-level panic
                // the same way).
                // `AssertUnwindSafe` is sound: the closure owns its read
                // parameters and touches no shared `&mut` state.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if test_delay_ms != 0 {
                        std::thread::sleep(std::time::Duration::from_millis(test_delay_ms));
                    }
                    if test_panic {
                        panic!("bitty test-injected kitty local worker panic");
                    }
                    read_kitty_local(
                        medium,
                        &name_owned,
                        data_size,
                        data_offset,
                        format_f,
                        width_s,
                        height_v,
                    )
                }));
                match outcome {
                    Ok(inner) => inner,
                    Err(_) => Err(KittyLocalReadError::Io("read")),
                }
            });
        match spawn {
            Ok(handle) => {
                self.kitty_local_in_flight = Some(KittyLocalInFlight {
                    request,
                    deadline,
                    handle,
                });
            }
            Err(_) => {
                // Spawn refused by the OS: fail the head closed now with
                // the same silent posture as a refused read (the caller
                // already continued; completion-time handling reports it).
                self.fail_kitty_local_request(&request, true);
            }
        }
    }

    /// Silent failure posture for one request that never ran (CTX-1110):
    /// spawn refusal or queue-full fast refusal. Display/frame/virtual log
    /// rate-limited (no stderr flood); queries answer `EINVAL:bad data`
    /// honoring `q=` (never stderr). `from_spawn` only selects the log
    /// wording.
    pub(crate) fn fail_kitty_local_request(
        &mut self,
        request: &KittyLocalRequest,
        from_spawn: bool,
    ) {
        let origin = request.origin;
        // Lifetime: a pane closed before failure handling drops everything.
        if let Some(token) = origin {
            if !self.pane_sessions.contains_key(&ViewId::new(token)) {
                return;
            }
        }
        let prev_origin = self.kitty_origin;
        // Pane completions run on the pane's grid (swapped in); primary
        // runs on the primary grid already in place.
        let mut swapped = false;
        if let Some(token) = origin {
            if let Some(sess) = self.pane_sessions.get_mut(&ViewId::new(token)) {
                std::mem::swap(&mut self.parser, &mut sess.parser);
                std::mem::swap(&mut self.state, &mut sess.state);
                std::mem::swap(&mut self.query_overlap, &mut sess.query_overlap);
                swapped = true;
            } else {
                return;
            }
        }
        self.kitty_origin = origin;
        match &request.op {
            KittyLocalOp::Query { control, .. } => {
                if control.quiet < 2 {
                    let reply =
                        crate::queries::kitty_probe_einval_reply(control.image_id, "bad data");
                    self.state.apply(&bitty_term_state::TerminalAction::Reply {
                        bytes: reply.into_boxed_slice(),
                    });
                }
            }
            KittyLocalOp::Display { .. }
            | KittyLocalOp::Virtual { .. }
            | KittyLocalOp::Frame { .. } => {
                if let Some(suppressed) = self.kitty_log.admit_now() {
                    let what = if from_spawn {
                        "spawn refused"
                    } else {
                        "queue full"
                    };
                    eprintln!(
                        "bitty: rejecting kitty local image ({what}): stored nothing{}",
                        super::log_throttle::suppressed_suffix(suppressed)
                    );
                }
            }
        }
        if swapped {
            if let Some(sess) = self
                .pane_sessions
                .get_mut(&ViewId::new(origin.expect("swapped implies origin")))
            {
                std::mem::swap(&mut self.parser, &mut sess.parser);
                std::mem::swap(&mut self.state, &mut sess.state);
                std::mem::swap(&mut self.query_overlap, &mut sess.query_overlap);
            }
        }
        self.kitty_origin = prev_origin;
        self.sync_mode_caches_to_focus();
    }

    /// Non-blocking pump for the async worker (CTX-1110): reaps finished
    /// orphans, completes or times out the in-flight read, then promotes
    /// the queue head. Never blocks, never panics on worker output.
    pub fn pump_kitty_local_completions(&mut self) {
        // Reap finished orphans first (bounded leak: hung threads that
        // finally exited release their slots here).
        let mut index = 0;
        while index < self.kitty_local_orphans.len() {
            if self.kitty_local_orphans[index].handle.is_finished() {
                let orphan = self.kitty_local_orphans.remove(index);
                let _ = orphan.handle.join();
            } else {
                index += 1;
            }
        }
        // Check the in-flight worker without blocking.
        let mut timed_out = false;
        // Take a finished worker out first (borrow ends before completion
        // handling, which needs `&mut self`).
        let finished = self
            .kitty_local_in_flight
            .as_ref()
            .is_some_and(|flight| flight.handle.is_finished());
        if finished {
            let flight = self.kitty_local_in_flight.take().expect("checked above");
            let result = match flight.handle.join() {
                Ok(inner) => inner,
                Err(_) => Err(KittyLocalReadError::Io("read")),
            };
            self.complete_kitty_local_request(&flight.request, result);
        } else if let Some(flight) = self.kitty_local_in_flight.as_ref() {
            if std::time::Instant::now() >= flight.deadline {
                timed_out = true;
            }
        }
        if timed_out {
            let flight = self.kitty_local_in_flight.take().expect("checked above");
            // On timeout answer silent failure honoring `q=` (S2 shape, no
            // stderr flood): display/frame/virtual rate-limited log, query
            // `EINVAL` unless `q=2`. The hung thread becomes a bounded
            // orphan: it still counts toward the caps until it exits.
            self.fail_kitty_local_timed_out(&flight.request);
            self.kitty_local_orphans.push(KittyLocalOrphan {
                origin: flight.request.origin,
                handle: flight.handle,
            });
            // Defense in depth only (unreachable through this path): the
            // enqueue gate holds the live threads at the global thread cap,
            // and both the timeout move (in-flight to orphan) and the
            // promote below (queue to in-flight) preserve it, so the thread
            // total cannot exceed the cap here. Retained so a future path
            // change that grows the total still detaches instead of leaking
            // unbounded (dropping the handle detaches the thread). At most
            // one timeout lands per pump, so one detach per pump suffices.
            if self.kitty_local_threads_total() > KITTY_LOCAL_MAX_PENDING_GLOBAL {
                if let Some(orphan) = self.kitty_local_orphans.pop() {
                    let _ = orphan;
                }
            }
        }
        self.promote_kitty_local_queue();
    }

    /// Timeout failure posture (CTX-1110): same silent shape as a refused
    /// read (rate-limited log for display/frame/virtual, `EINVAL` honoring
    /// `q=` for queries), worded as a timeout.
    fn fail_kitty_local_timed_out(&mut self, request: &KittyLocalRequest) {
        let origin = request.origin;
        if let Some(token) = origin {
            if !self.pane_sessions.contains_key(&ViewId::new(token)) {
                return;
            }
        }
        let prev_origin = self.kitty_origin;
        let mut swapped = false;
        if let Some(token) = origin {
            if let Some(sess) = self.pane_sessions.get_mut(&ViewId::new(token)) {
                std::mem::swap(&mut self.parser, &mut sess.parser);
                std::mem::swap(&mut self.state, &mut sess.state);
                std::mem::swap(&mut self.query_overlap, &mut sess.query_overlap);
                swapped = true;
            } else {
                return;
            }
        }
        self.kitty_origin = origin;
        match &request.op {
            KittyLocalOp::Query { control, .. } => {
                if control.quiet < 2 {
                    let reply =
                        crate::queries::kitty_probe_einval_reply(control.image_id, "bad data");
                    self.state.apply(&bitty_term_state::TerminalAction::Reply {
                        bytes: reply.into_boxed_slice(),
                    });
                }
            }
            KittyLocalOp::Display { .. }
            | KittyLocalOp::Virtual { .. }
            | KittyLocalOp::Frame { .. } => {
                if let Some(suppressed) = self.kitty_log.admit_now() {
                    eprintln!(
                        "bitty: rejecting kitty local image (read timed out): stored nothing{}",
                        super::log_throttle::suppressed_suffix(suppressed)
                    );
                }
            }
        }
        if swapped {
            if let Some(sess) = self
                .pane_sessions
                .get_mut(&ViewId::new(origin.expect("swapped implies origin")))
            {
                std::mem::swap(&mut self.parser, &mut sess.parser);
                std::mem::swap(&mut self.state, &mut sess.state);
                std::mem::swap(&mut self.query_overlap, &mut sess.query_overlap);
            }
        }
        self.kitty_origin = prev_origin;
        self.sync_mode_caches_to_focus();
    }

    /// Applies one finished worker result on the requesting origin's grid
    /// (CTX-1110 completion).
    ///
    /// Lifetime: a pane closed mid-read drops its result (no
    /// use-after-close, no store, no reply). Ordering: single-flight FIFO
    /// means completions already run in enqueue order, so frame N+1 cannot
    /// display before frame N of the same image. Panic isolation: panics
    /// arrive here as `Io("read")` refusals, never propagated.
    ///
    /// Residuals (documented, conservative): completions anchor at the
    /// enqueue-time cursor/scrollback captured when the APC drained (see
    /// `KittyLocalRequest::anchor_col`), reproducing the sync order — place
    /// at the APC cell, then advance past the image from that anchor — even
    /// when prompt or output text drained after the enqueue moved the live
    /// cursor. The worker behind a slow disk finishes on a later poll; the
    /// anchor (not the completion-time cursor) decides placement, so drift
    /// is bounded by construction rather than by poll timing. Sequencing: a
    /// delete/clear/retire that lands while a transmit pends advances the
    /// origin's delete generation, so the stale storing completion drops
    /// (delete-wins, silent) instead of resurrecting the image;
    /// control-only composes racing a pending frame still fail closed to
    /// prior state (`NoAnimationTarget`) rather than waiting.
    fn complete_kitty_local_request(
        &mut self,
        request: &KittyLocalRequest,
        result: Result<Vec<u8>, KittyLocalReadError>,
    ) {
        let origin = request.origin;
        // Lifetime gate first: no grid touch when the origin is gone.
        if let Some(token) = origin {
            if !self.pane_sessions.contains_key(&ViewId::new(token)) {
                return;
            }
        }
        // Delete-wins sequencing (CTX-1110 fix pass): a transmit enqueued
        // before a delete/clear/retire for this origin must not resurrect
        // the image when its worker finishes after the delete (the delete
        // no-oped on the not-yet-stored image, while sync order stores
        // then deletes and yields absent). Queries never store and frames
        // re-resolve their target fail-closed, so only storing completions
        // (display/virtual) drop here — silently, like a closed origin
        // (a racing delete is routine, not diagnostics).
        if matches!(
            request.op,
            KittyLocalOp::Display { .. } | KittyLocalOp::Virtual { .. }
        ) && request.delete_epoch != self.kitty_local_delete_epoch_for(origin)
        {
            return;
        }
        let prev_origin = self.kitty_origin;
        let mut swapped = false;
        if let Some(token) = origin {
            if let Some(sess) = self.pane_sessions.get_mut(&ViewId::new(token)) {
                std::mem::swap(&mut self.parser, &mut sess.parser);
                std::mem::swap(&mut self.state, &mut sess.state);
                std::mem::swap(&mut self.query_overlap, &mut sess.query_overlap);
                swapped = true;
            } else {
                return;
            }
        }
        self.kitty_origin = origin;
        match &request.op {
            KittyLocalOp::Display {
                format_f,
                width_s,
                height_v,
                action_a,
                cols_c,
                rows_r,
                cursor_movement_c,
                wire_image,
                wire_placement,
            } => match result {
                Ok(bytes) => {
                    if let Err(err) = self.kitty_display_image_owned_with_wire_at_anchor(
                        *format_f,
                        *width_s,
                        *height_v,
                        *action_a,
                        *cols_c,
                        *rows_r,
                        *cursor_movement_c,
                        bytes.into_boxed_slice(),
                        0,
                        *wire_image,
                        *wire_placement,
                        request.anchor_col,
                        request.anchor_row,
                        request.scrollback_base,
                    ) {
                        if let Some(suppressed) = self.kitty_log.admit_now() {
                            eprintln!(
                                "bitty: rejecting kitty local image ({err}): stored nothing{}",
                                super::log_throttle::suppressed_suffix(suppressed)
                            );
                        }
                    }
                }
                Err(err) => {
                    if let Some(suppressed) = self.kitty_log.admit_now() {
                        eprintln!(
                            "bitty: rejecting kitty local image ({err}): stored nothing{}",
                            super::log_throttle::suppressed_suffix(suppressed)
                        );
                    }
                }
            },
            KittyLocalOp::Virtual {
                format_f,
                width_s,
                height_v,
                cols_c,
                rows_r,
                wire_image,
                wire_placement,
                has_parent,
            } => match result {
                Ok(bytes) => {
                    if let Err(err) = self.kitty_display_virtual_owned_with_wire_at_base(
                        *format_f,
                        *width_s,
                        *height_v,
                        *cols_c,
                        *rows_r,
                        bytes.into_boxed_slice(),
                        *wire_image,
                        *wire_placement,
                        *has_parent,
                        request.scrollback_base,
                    ) {
                        if let Some(suppressed) = self.kitty_log.admit_now() {
                            eprintln!(
                                "bitty: rejecting kitty local virtual image ({err}): stored nothing{}",
                                super::log_throttle::suppressed_suffix(suppressed)
                            );
                        }
                    }
                }
                Err(err) => {
                    if let Some(suppressed) = self.kitty_log.admit_now() {
                        eprintln!(
                            "bitty: rejecting kitty local virtual image ({err}): stored nothing{}",
                            super::log_throttle::suppressed_suffix(suppressed)
                        );
                    }
                }
            },
            KittyLocalOp::Query {
                format_f,
                width_s,
                height_v,
                control,
            } => match result {
                Ok(bytes) => {
                    self.answer_kitty_query(*format_f, *width_s, *height_v, &bytes, *control);
                }
                Err(_) => {
                    if control.quiet >= 2 {
                        // Suppressed like the synchronous path.
                    } else {
                        let reply =
                            crate::queries::kitty_probe_einval_reply(control.image_id, "bad data");
                        self.state.apply(&bitty_term_state::TerminalAction::Reply {
                            bytes: reply.into_boxed_slice(),
                        });
                    }
                }
            },
            KittyLocalOp::Frame {
                format_f,
                width_s,
                height_v,
                control,
                action,
            } => match result {
                Ok(bytes) => {
                    if let Err(err) = self.kitty_ingest_frame_bytes(
                        *format_f, *width_s, *height_v, &bytes, control, action,
                    ) {
                        if let Some(suppressed) = self.kitty_log.admit_now() {
                            eprintln!(
                                "bitty: rejecting kitty animation frame ({err}): stored nothing{}",
                                super::log_throttle::suppressed_suffix(suppressed)
                            );
                        }
                    }
                }
                Err(err) => {
                    if let Some(suppressed) = self.kitty_log.admit_now() {
                        eprintln!(
                            "bitty: rejecting kitty animation frame ({err}): stored nothing{}",
                            super::log_throttle::suppressed_suffix(suppressed)
                        );
                    }
                }
            },
        }
        if swapped {
            if let Some(sess) = self
                .pane_sessions
                .get_mut(&ViewId::new(origin.expect("swapped implies origin")))
            {
                std::mem::swap(&mut self.parser, &mut sess.parser);
                std::mem::swap(&mut self.state, &mut sess.state);
                std::mem::swap(&mut self.query_overlap, &mut sess.query_overlap);
            }
        }
        self.kitty_origin = prev_origin;
        self.sync_mode_caches_to_focus();
    }
}

impl Runtime {
    /// Decodes, stores and places a Kitty image named by a local medium.
    ///
    /// Reads the `t=f`/`t=t`/`t=s` object named by `name` (bounded, fail
    /// closed), then decodes, stores and places through
    /// [`Self::kitty_display_image_owned_with_wire`], so per-origin quotas
    /// (S5), alternate-screen suppression, cursor advance, and raster-cache
    /// invalidation apply unchanged. `control` carries the medium plus
    /// `S=`/`O=` and the wire `i=`/`p=` identity.
    ///
    /// # Errors
    ///
    /// [`KittyImageError::LocalRead`] when the sandbox refuses the name,
    /// the platform, the open, or the bounded read; otherwise the same
    /// taxonomy as [`Self::kitty_display_image_owned_with_wire`]. Failures
    /// store nothing and (for `t=t`/`t=s`) delete nothing on refusal — the
    /// unlink runs only after a successful bounded read.
    #[allow(clippy::too_many_arguments)]
    pub fn kitty_display_local_owned_with_wire(
        &mut self,
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        action_a: Option<char>,
        cols_c: u16,
        rows_r: u16,
        cursor_movement_c: u8,
        name: Box<[u8]>,
        control: bitty_vt::KittyControlKeys,
    ) -> Result<KittyDisplayOutcome, KittyImageError> {
        let bytes = read_kitty_local(
            control.medium,
            &name,
            control.data_size,
            control.data_offset,
            format_f,
            width_s,
            height_v,
        )
        .map_err(KittyImageError::LocalRead)?;
        self.kitty_display_image_owned_with_wire(
            format_f,
            width_s,
            height_v,
            action_a,
            cols_c,
            rows_r,
            cursor_movement_c,
            bytes.into_boxed_slice(),
            0,
            control.image_id,
            control.placement_id,
        )
    }

    /// Decodes, stores and registers a `U=1` virtual prototype named by a
    /// local medium (S3 read half over the S4 registration seam).
    ///
    /// Same behavior as [`Self::kitty_display_local_owned_with_wire`] except
    /// the read bytes register a prototype through
    /// [`Self::kitty_display_virtual_owned_with_wire`] instead of anchoring
    /// a blit placement.
    ///
    /// # Errors
    ///
    /// Same taxonomy as [`Self::kitty_display_local_owned_with_wire`].
    #[allow(clippy::too_many_arguments)]
    pub fn kitty_display_local_virtual_owned_with_wire(
        &mut self,
        format_f: u32,
        width_s: Option<u32>,
        height_v: Option<u32>,
        cols_c: u16,
        rows_r: u16,
        name: Box<[u8]>,
        control: bitty_vt::KittyControlKeys,
    ) -> Result<KittyDisplayOutcome, KittyImageError> {
        let bytes = read_kitty_local(
            control.medium,
            &name,
            control.data_size,
            control.data_offset,
            format_f,
            width_s,
            height_v,
        )
        .map_err(KittyImageError::LocalRead)?;
        self.kitty_display_virtual_owned_with_wire(
            format_f,
            width_s,
            height_v,
            cols_c,
            rows_r,
            bytes.into_boxed_slice(),
            control.image_id,
            control.placement_id,
            control.has_parent(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_shape_matrix() {
        use bitty_vt::KittyMedium::{Direct, File, SharedMemory, TempFile};
        // Direct misrouted here always fails closed.
        assert_eq!(
            read_kitty_local(Direct, b"/tmp/x", 0, 0, 100, None, None),
            Err(KittyLocalReadError::BadName)
        );
        for (medium, name, ok) in [
            (File, b"/tmp/x.png".as_slice(), true),
            (File, b"relative/x.png".as_slice(), true),
            (File, b"".as_slice(), false),
            (File, b"/tmp/../etc/passwd".as_slice(), false),
            (File, b"..".as_slice(), false),
            (File, b"/a\x00b".as_slice(), false),
            (TempFile, b"/tmp/tty-graphics-protocol-1".as_slice(), true),
            (TempFile, b"/tmp/other-file".as_slice(), false),
            (TempFile, b"/tmp/../tty-graphics-protocol".as_slice(), false),
            (SharedMemory, b"/kitty-123".as_slice(), true),
            (SharedMemory, b"noslash".as_slice(), false),
            (SharedMemory, b"/".as_slice(), false),
            (SharedMemory, b"/a/b".as_slice(), false),
            (SharedMemory, b"".as_slice(), false),
        ] {
            assert_eq!(
                check_name_shape(medium, name),
                if ok {
                    Ok(())
                } else {
                    Err(KittyLocalReadError::BadName)
                },
                "medium {medium:?} name {name:?}"
            );
        }
        // Overlong names fail closed on every medium.
        let big = vec![b'a'; KITTY_APC_PATH_MAX_BYTES + 1];
        for medium in [File, TempFile, SharedMemory] {
            assert_eq!(
                check_name_shape(medium, &big),
                Err(KittyLocalReadError::BadName)
            );
        }
        // Overlong shm name (past NAME_MAX) fails closed.
        let mut shm = b"/".to_vec();
        shm.extend_from_slice(&vec![b'a'; KITTY_APC_SHM_NAME_MAX + 1]);
        assert_eq!(
            check_name_shape(SharedMemory, &shm),
            Err(KittyLocalReadError::BadName)
        );
    }

    #[test]
    fn read_cap_arithmetic_is_checked() {
        // Raw claims bind the read to the exact declared size.
        assert_eq!(read_cap_bytes(32, Some(2), Some(2), 0), Ok(16));
        assert_eq!(read_cap_bytes(24, Some(2), Some(1), 0), Ok(6));
        // `S=` narrows, never widens past the cap.
        assert_eq!(read_cap_bytes(32, Some(2), Some(2), 8), Ok(8));
        assert_eq!(read_cap_bytes(100, None, None, 0), Ok(KITTY_LOCAL_READ_CAP));
        // Oversize declarations refuse before any I/O.
        assert_eq!(
            read_cap_bytes(32, Some(9000), Some(1), 0),
            Err(KittyLocalReadError::TooLarge)
        );
        assert_eq!(
            read_cap_bytes(32, Some(5000), Some(5000), 0),
            Err(KittyLocalReadError::TooLarge)
        );
        assert_eq!(
            read_cap_bytes(100, None, None, u32::MAX),
            Err(KittyLocalReadError::TooLarge)
        );
    }

    #[test]
    fn offset_size_overflow_refused_before_io() {
        use bitty_vt::KittyMedium::File;
        // `O=` + `S=` overflow refuses without touching the filesystem.
        assert_eq!(
            read_kitty_local(File, b"/tmp/x", u32::MAX, u32::MAX, 100, None, None),
            Err(KittyLocalReadError::TooLarge)
        );
        // Offsets past the byte cap refuse without I/O.
        assert_eq!(
            read_kitty_local(
                File,
                b"/tmp/x",
                0,
                KITTY_LOCAL_READ_CAP as u32 + 1,
                100,
                None,
                None
            ),
            Err(KittyLocalReadError::OffsetOutOfRange)
        );
    }

    #[test]
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    fn temp_roots_allowlist_is_nonempty() {
        assert!(
            !temp_roots().is_empty(),
            "at least the OS temp dir must canonicalize"
        );
        assert!(temp_roots().iter().all(|root| root.is_absolute()));
    }

    // CTX-1110 async worker tests: hermetic and fast (tiny timeouts plus
    // fault-injection hooks, never real multi-second sleeps or hung mounts).
    // File fixtures only run on Unix (every file caller is `#[cfg(unix)]`;
    // the caps test uses a byte literal), so the counter, fixture, and path
    // helpers are Unix-gated to stay `dead_code`-clean on Windows.
    #[cfg(unix)]
    static ASYNC_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn async_runtime() -> crate::runtime::Runtime {
        crate::runtime::Runtime::with_defaults().expect("defaults must build")
    }

    #[cfg(unix)]
    fn async_fixture(tag: &str, bytes: &[u8]) -> std::path::PathBuf {
        let n = ASYNC_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("bitty-ctx1110-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir must build");
        let file = dir.join("img.bin");
        std::fs::write(&file, bytes).expect("fixture must write");
        file
    }

    #[cfg(unix)]
    fn async_path_bytes(path: &std::path::Path) -> Vec<u8> {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }

    fn async_control(name_len: usize) -> bitty_vt::KittyControlKeys {
        let _ = name_len;
        bitty_vt::KittyControlKeys {
            medium: bitty_vt::KittyMedium::File,
            ..Default::default()
        }
    }

    #[test]
    #[cfg(unix)]
    fn async_timeout_fires_hermetic_fast() {
        // Fault-injected hung filesystem: the worker sleeps past a tiny
        // deadline. Timeout degrades to silent refusal (no store), fast.
        let file = async_fixture("timeout", &[0xFF, 0x00, 0x00, 0xFF]);
        let name = async_path_bytes(&file);

        let mut rt = async_runtime();
        rt.kitty_local_test_timeout_ms = 30;
        rt.kitty_local_test_delay_ms = 500;
        let control = async_control(name.len());
        assert!(rt.enqueue_kitty_local_display(
            32,
            Some(1),
            Some(1),
            None,
            1,
            1,
            0,
            &name,
            control
        ));
        // Past the 30 ms deadline, well before the 500 ms worker sleep.
        std::thread::sleep(std::time::Duration::from_millis(60));
        rt.pump_kitty_local_completions();
        rt.kitty_local_test_timeout_ms = 0;
        rt.kitty_local_test_delay_ms = 0;
        assert_eq!(
            rt.kitty_image_count(),
            0,
            "timed-out display stores nothing"
        );
        assert_eq!(
            rt.kitty_placement_count(),
            0,
            "timed-out display places nothing"
        );
        // Orphan detached on drop; no hang.
        let _ = std::fs::remove_dir_all(file.parent().expect("temp parent"));
    }

    #[test]
    #[cfg(unix)]
    fn async_query_timeout_honors_quiet() {
        // Hung probe with `q=0` answers `EINVAL:bad data`; `q=2`
        // suppresses. Both hermetic and fast via the same hooks.
        let file = async_fixture("qtimeout", &[0xFF, 0x00, 0x00, 0xFF]);
        let name = async_path_bytes(&file);

        let mut rt = async_runtime();
        rt.kitty_local_test_timeout_ms = 30;
        rt.kitty_local_test_delay_ms = 500;
        let mut control = async_control(name.len());
        control.image_id = 31;
        control.quiet = 0;
        rt.enqueue_kitty_local_query(100, None, None, &name, control);
        std::thread::sleep(std::time::Duration::from_millis(60));
        rt.pump_kitty_local_completions();
        let replies: Vec<Vec<u8>> = rt.take_replies().iter().map(|r| r.to_vec()).collect();
        assert_eq!(
            replies,
            vec![b"\x1b_Gi=31;EINVAL:bad data\x1b\\".to_vec()],
            "timed-out probe answers EINVAL honoring q=0"
        );

        let mut rt = async_runtime();
        rt.kitty_local_test_timeout_ms = 30;
        rt.kitty_local_test_delay_ms = 500;
        let mut control = async_control(name.len());
        control.image_id = 31;
        control.quiet = 2;
        rt.enqueue_kitty_local_query(100, None, None, &name, control);
        std::thread::sleep(std::time::Duration::from_millis(60));
        rt.pump_kitty_local_completions();
        assert!(
            rt.take_replies().is_empty(),
            "q=2 suppresses timeout failures"
        );

        let _ = std::fs::remove_dir_all(file.parent().expect("temp parent"));
    }

    #[test]
    fn async_queue_caps_bound_global_and_per_origin() {
        // Split caps (CTX-1110 fix pass, CodeRabbit Major #1): blocked
        // workers (injected delay, generous deadline) fill the bounded
        // QUEUE without any I/O timing dependence. Thread caps (2 per
        // origin / 8 global in-flight+orphan) stay tight; queue caps (32
        // per origin / 64 global) admit bursts a 10-frame animation needs.
        let mut rt = async_runtime();
        rt.kitty_local_test_timeout_ms = 5000;
        rt.kitty_local_test_delay_ms = 300;
        let name = b"/tmp/bitty-ctx1110-caps.bin".as_slice();
        let control = async_control(name.len());
        // Burst tolerance: ten frames from one origin are all admitted
        // (one in flight, nine queued) — the old unified cap of two
        // dropped eight of them silently.
        for _ in 0..10 {
            assert!(rt.enqueue_kitty_local_display(
                32,
                Some(1),
                Some(1),
                None,
                1,
                1,
                0,
                name,
                control
            ));
        }
        assert_eq!(rt.kitty_local_threads_total(), 1);
        assert_eq!(rt.kitty_local_queued_for_origin(None), 9);
        // Per-origin queue cap: 32 queued for the primary (1 in flight +
        // 32 queued = 33 admitted), the 34th refuses.
        for _ in 10..33 {
            assert!(rt.enqueue_kitty_local_display(
                32,
                Some(1),
                Some(1),
                None,
                1,
                1,
                0,
                name,
                control
            ));
        }
        assert_eq!(rt.kitty_local_queued_for_origin(None), 32);
        assert_eq!(
            rt.kitty_local_pending_count(),
            33,
            "primary holds 1 in flight + 32 queued"
        );
        assert!(
            !rt.enqueue_kitty_local_display(32, Some(1), Some(1), None, 1, 1, 0, name, control),
            "per-origin queue cap refuses past 32 queued"
        );
        // Other origins still have slots (no starvation): origin 7 queues
        // its own 32, and the 33rd refuses on the per-origin cap.
        rt.kitty_origin = Some(7);
        for _ in 0..32 {
            assert!(rt.enqueue_kitty_local_display(
                32,
                Some(1),
                Some(1),
                None,
                1,
                1,
                0,
                name,
                control
            ));
        }
        assert!(
            !rt.enqueue_kitty_local_display(32, Some(1), Some(1), None, 1, 1, 0, name, control),
            "per-origin queue cap refuses the 33rd queued for origin 7"
        );
        rt.kitty_origin = None;
        // Global queue cap: 32 + 32 = 64 queued; a fresh origin refuses.
        assert_eq!(rt.kitty_local_queued_total(), 64);
        rt.kitty_origin = Some(11);
        assert!(
            !rt.enqueue_kitty_local_display(32, Some(1), Some(1), None, 1, 1, 0, name, control),
            "global queue cap refuses past 64 queued"
        );
        rt.kitty_origin = None;
        // Blocked workers detach on drop; the test itself never waited.
    }

    #[test]
    #[cfg(unix)]
    fn async_thread_caps_refuse_when_orphans_saturate() {
        // Thread caps stay tight (CTX-1110 fix pass, CodeRabbit Major #1):
        // two timed-out orphans for one origin refuse the next enqueue on
        // the per-origin thread cap even though the queue has room.
        let file = async_fixture("threadcap", &[0xFF, 0x00, 0x00, 0xFF]);
        let name = async_path_bytes(&file);
        let mut rt = async_runtime();
        rt.kitty_local_test_timeout_ms = 30;
        rt.kitty_local_test_delay_ms = 500;
        let control = async_control(name.len());
        for _ in 0..2 {
            assert!(rt.enqueue_kitty_local_display(
                32,
                Some(1),
                Some(1),
                None,
                1,
                1,
                0,
                &name,
                control
            ));
            std::thread::sleep(std::time::Duration::from_millis(60));
            rt.pump_kitty_local_completions();
        }
        rt.kitty_local_test_timeout_ms = 0;
        rt.kitty_local_test_delay_ms = 0;
        assert_eq!(rt.kitty_local_threads_for_origin(None), 2);
        assert_eq!(rt.kitty_local_queued_total(), 0);
        assert!(
            !rt.enqueue_kitty_local_display(32, Some(1), Some(1), None, 1, 1, 0, &name, control),
            "per-origin thread cap refuses with two orphans outstanding"
        );
        let _ = std::fs::remove_dir_all(file.parent().expect("temp parent"));
    }

    #[test]
    #[cfg(unix)]
    fn async_worker_wakes_loop_on_completion() {
        // CodeRabbit Major #2: the worker invokes `pty_waker` when its read
        // finishes, so a `ControlFlow::Wait` loop applies the completion
        // promptly instead of sitting until the next incidental event.
        let file = async_fixture("waker", &[0xFF, 0x00, 0x00, 0xFF]);
        let name = async_path_bytes(&file);
        let mut rt = async_runtime();
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let witness = std::sync::Arc::clone(&fired);
        rt.set_pty_waker(std::sync::Arc::new(move || {
            witness.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        let control = async_control(name.len());
        assert!(rt.enqueue_kitty_local_display(
            32,
            Some(1),
            Some(1),
            None,
            1,
            1,
            0,
            &name,
            control
        ));
        for _ in 0..200 {
            rt.pump_kitty_local_completions();
            if rt.kitty_local_pending_count() == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(rt.kitty_image_count(), 1, "fast read still lands");
        assert!(
            fired.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "worker must wake the loop on completion"
        );
        let _ = std::fs::remove_dir_all(file.parent().expect("temp parent"));
    }

    #[test]
    #[cfg(unix)]
    fn async_worker_wakes_loop_on_panic_path() {
        // CodeRabbit Major #2, early-exit path: the injected panic used to
        // fire before `catch_unwind`, skipping a wake placed only after it.
        // The `Drop` guard wakes on every exit, including this one.
        let file = async_fixture("waker-panic", &[0xFF, 0x00, 0x00, 0xFF]);
        let name = async_path_bytes(&file);
        let mut rt = async_runtime();
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let witness = std::sync::Arc::clone(&fired);
        rt.set_pty_waker(std::sync::Arc::new(move || {
            witness.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        rt.kitty_local_test_panic = true;
        let control = async_control(name.len());
        assert!(rt.enqueue_kitty_local_display(
            32,
            Some(1),
            Some(1),
            None,
            1,
            1,
            0,
            &name,
            control
        ));
        for _ in 0..200 {
            rt.pump_kitty_local_completions();
            if rt.kitty_local_pending_count() == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        rt.kitty_local_test_panic = false;
        assert_eq!(rt.kitty_image_count(), 0, "panicked read stores nothing");
        assert!(
            fired.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "worker must wake the loop even on the panic path"
        );
        let _ = std::fs::remove_dir_all(file.parent().expect("temp parent"));
    }

    #[test]
    fn async_deadline_tracks_in_flight_worker() {
        // CodeRabbit Major #2: the in-flight worker publishes its wall-clock
        // bound so the app loop can arm `set_wait_until` at it. Queued-only
        // and idle states publish `None`.
        let mut rt = async_runtime();
        assert_eq!(
            rt.kitty_local_deadline(),
            None,
            "idle publishes no deadline"
        );
        rt.kitty_local_test_timeout_ms = 5000;
        rt.kitty_local_test_delay_ms = 300;
        let name = b"/tmp/bitty-ctx1110-deadline.bin".as_slice();
        let control = async_control(name.len());
        assert!(rt.enqueue_kitty_local_display(32, Some(1), Some(1), None, 1, 1, 0, name, control));
        let before = std::time::Instant::now();
        let deadline = rt
            .kitty_local_deadline()
            .expect("in-flight worker must publish a deadline");
        // Generous 5 s test override: strictly in the future, bounded above
        // by the configured timeout (the app's `min` over deadlines can only
        // wake at or before this instant).
        assert!(deadline > before, "deadline must be in the future");
        assert!(
            deadline <= before + std::time::Duration::from_millis(5000),
            "deadline must respect the configured timeout"
        );
        // Blocked worker detaches on drop; the test itself never waited.
    }

    #[test]
    #[cfg(unix)]
    fn async_completion_places_at_enqueue_anchor() {
        // CodeRabbit Major #3 (icat-then-prompt): the completion places at
        // the enqueue-time anchor even though prompt text drained after the
        // enqueue moved the live cursor — reproducing the sync order (place
        // at APC cursor, advance past the image from the anchor).
        let file = async_fixture("anchor", &[0xFF, 0x00, 0x00, 0xFF]);
        let name = async_path_bytes(&file);
        let mut rt = async_runtime();
        // Cursor starts at (0,0) with an empty scrollback: the enqueue
        // captures that anchor.
        let control = async_control(name.len());
        assert!(rt.enqueue_kitty_local_display(
            32,
            Some(1),
            Some(1),
            None,
            1,
            1,
            0,
            &name,
            control
        ));
        // Prompt text drains before the worker finishes: the live cursor
        // advances to (2,0) and the grid holds the prompt.
        rt.handle_pty_bytes(b"AB");
        assert_eq!(rt.state().cursor().position.col, 2);
        assert_eq!(rt.state().cursor().position.row, 0);
        for _ in 0..200 {
            rt.pump_kitty_local_completions();
            if rt.kitty_local_pending_count() == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(rt.kitty_image_count(), 1, "anchored read still lands");
        assert_eq!(rt.kitty_placement_count(), 1);
        // Placement sits at the enqueue-time anchor (0,0) with its base —
        // not at the live cursor (2,0) where the prompt printed.
        let (anchor_col, anchor_row, base) = {
            let placement = rt
                .kitty_images
                .placements()
                .next()
                .expect("placement must exist");
            (
                placement.anchor_col,
                placement.anchor_row,
                placement.scrollback_base,
            )
        };
        assert_eq!((anchor_col, anchor_row), (0, 0));
        assert_eq!(base, 0);
        // Cursor advanced past the 1x1 image FROM the anchor (sync order:
        // (0,0) + 1 col + 1 row), not from the live cursor (which would
        // land at (3,1)).
        let cursor = rt.state().cursor().position;
        assert_eq!((cursor.col, cursor.row), (1, 1));
        // Prompt text intact in the grid: the blit overlays, never erases.
        let row = rt.state().live_grid_row(0).expect("row 0 must exist");
        assert_eq!(row[0].glyph, 'A');
        assert_eq!(row[1].glyph, 'B');
        let _ = std::fs::remove_dir_all(file.parent().expect("temp parent"));
    }

    #[test]
    #[cfg(unix)]
    fn async_pane_closed_mid_read_drops_result() {
        // Lifetime: an origin with no live pane session drops its result
        // (no use-after-close, no store, no reply).
        let file = async_fixture("gone", &[0xFF, 0x00, 0x00, 0xFF]);
        let name = async_path_bytes(&file);
        let mut rt = async_runtime();
        rt.kitty_origin = Some(9999);
        let control = async_control(name.len());
        assert!(rt.enqueue_kitty_local_display(
            32,
            Some(1),
            Some(1),
            None,
            1,
            1,
            0,
            &name,
            control
        ));
        rt.kitty_origin = None;
        for _ in 0..200 {
            rt.pump_kitty_local_completions();
            if rt.kitty_local_pending_count() == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(rt.kitty_image_count(), 0, "gone origin stores nothing");
        assert_eq!(rt.kitty_placement_count(), 0);

        let mut rt = async_runtime();
        rt.kitty_origin = Some(9999);
        let mut control = async_control(name.len());
        control.image_id = 31;
        rt.enqueue_kitty_local_query(100, None, None, &name, control);
        rt.kitty_origin = None;
        for _ in 0..200 {
            rt.pump_kitty_local_completions();
            if rt.kitty_local_pending_count() == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(rt.take_replies().is_empty(), "gone origin answers nothing");
        let _ = std::fs::remove_dir_all(file.parent().expect("temp parent"));
    }

    #[test]
    #[cfg(unix)]
    fn async_worker_panic_degrades_to_refusal() {
        // Panic isolation: a panicking worker refuses without taking down
        // the poll, and the next read still works.
        let file = async_fixture("panic", &[0xFF, 0x00, 0x00, 0xFF]);
        let name = async_path_bytes(&file);
        let mut rt = async_runtime();
        rt.kitty_local_test_panic = true;
        let control = async_control(name.len());
        assert!(rt.enqueue_kitty_local_display(
            32,
            Some(1),
            Some(1),
            None,
            1,
            1,
            0,
            &name,
            control
        ));
        for _ in 0..200 {
            rt.pump_kitty_local_completions();
            if rt.kitty_local_pending_count() == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        rt.kitty_local_test_panic = false;
        assert_eq!(rt.kitty_image_count(), 0, "panicked read stores nothing");
        // Recovery: the queue still works after a panic.
        let control = async_control(name.len());
        assert!(rt.enqueue_kitty_local_display(
            32,
            Some(1),
            Some(1),
            None,
            1,
            1,
            0,
            &name,
            control
        ));
        for _ in 0..200 {
            rt.pump_kitty_local_completions();
            if rt.kitty_local_pending_count() == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(rt.kitty_image_count(), 1, "queue recovers after panic");
        let _ = std::fs::remove_dir_all(file.parent().expect("temp parent"));
    }

    #[test]
    #[cfg(unix)]
    fn async_fifo_stores_in_order() {
        // Ordering smoke: single-flight FIFO completes both enqueued
        // displays (frame N+1 cannot pass frame N by construction).
        let file_a = async_fixture("fifo-a", &[0xFF, 0x00, 0x00, 0xFF]);
        let file_b = async_fixture("fifo-b", &[0x00, 0xFF, 0x00, 0xFF]);
        let name_a = async_path_bytes(&file_a);
        let name_b = async_path_bytes(&file_b);
        let mut rt = async_runtime();
        let control = async_control(name_a.len());
        assert!(rt.enqueue_kitty_local_display(
            32,
            Some(1),
            Some(1),
            None,
            1,
            1,
            0,
            &name_a,
            control
        ));
        assert!(rt.enqueue_kitty_local_display(
            32,
            Some(1),
            Some(1),
            None,
            1,
            1,
            0,
            &name_b,
            control
        ));
        for _ in 0..200 {
            rt.pump_kitty_local_completions();
            if rt.kitty_local_pending_count() == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // Per-origin cap is two, so both enqueued; FIFO completes both.
        assert_eq!(rt.kitty_image_count(), 2);
        assert_eq!(rt.kitty_placement_count(), 2);
        let _ = std::fs::remove_dir_all(file_a.parent().expect("temp parent"));
        let _ = std::fs::remove_dir_all(file_b.parent().expect("temp parent"));
    }

    #[test]
    #[cfg(unix)]
    fn async_delete_before_completion_drops_stale() {
        // Transmit-then-delete resurrection (CTX-1110 fix pass): a display
        // enqueued on a hung filesystem, deleted before its worker wakes,
        // must not resurrect on completion. Sync order stores then deletes
        // and yields absent; the delete no-ops here on the not-yet-stored
        // image, so the delete generation is what invalidates the stale
        // completion. The worker still runs to completion (generous
        // deadline, injected delay): only the storing drops — silently,
        // with no placement and no reply.
        let file = async_fixture("del-race", &[0xFF, 0x00, 0x00, 0xFF]);
        let name = async_path_bytes(&file);
        let mut rt = async_runtime();
        rt.kitty_local_test_timeout_ms = 5000;
        rt.kitty_local_test_delay_ms = 300;
        let mut control = async_control(name.len());
        control.image_id = 41;
        assert!(rt.enqueue_kitty_local_display(
            32,
            Some(1),
            Some(1),
            None,
            1,
            1,
            0,
            &name,
            control
        ));
        // Delete names the wire id before the worker wakes: a no-op on
        // the store (removed 0) that still advances the generation.
        assert_eq!(rt.kitty_delete_rendered_image(41, None), 0);
        // Pump past the worker's completion (300 ms delay, 5 s deadline).
        for _ in 0..200 {
            rt.pump_kitty_local_completions();
            if rt.kitty_local_pending_count() == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(rt.kitty_local_pending_count(), 0, "worker must finish");
        assert_eq!(rt.kitty_image_count(), 0, "stale completion stores nothing");
        assert_eq!(
            rt.kitty_placement_count(),
            0,
            "stale completion places nothing"
        );
        assert!(
            rt.take_replies().is_empty(),
            "stale display completion replies nothing"
        );
        // A transmit enqueued after the delete stamps the new generation
        // and still lands: delete-wins, no wedge.
        let mut control = async_control(name.len());
        control.image_id = 41;
        assert!(rt.enqueue_kitty_local_display(
            32,
            Some(1),
            Some(1),
            None,
            1,
            1,
            0,
            &name,
            control
        ));
        for _ in 0..200 {
            rt.pump_kitty_local_completions();
            if rt.kitty_local_pending_count() == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(rt.kitty_image_count(), 1, "post-delete transmit lands");
        assert_eq!(rt.kitty_placement_count(), 1);
        let _ = std::fs::remove_dir_all(file.parent().expect("temp parent"));
    }
}
