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

    /// Answers one file-backed Kitty `a=q` probe with at most one bounded
    /// reply (S3 read half over the S2 answer seam, #1849).
    ///
    /// `I=` number lookups ignore the named object (like direct streams:
    /// delegated with an empty payload, which the verdict treats as a
    /// status lookup); queries without `i=`/`I=` stay silent (like kitty).
    /// Spec probes (`i=` non-zero) read the named object first (bounded,
    /// fail closed) and test-load the *read* bytes through
    /// [`Self::answer_kitty_query`], so `OK`/`EINVAL`/`ENOSPC` verdicts and
    /// `q=` suppression apply unchanged. Read failures answer
    /// `EINVAL:bad data` as silent protocol replies honoring `q=` (never
    /// stderr), so probing clients never flood diagnostics. Queries never
    /// store, place, or evict.
    pub(crate) fn answer_kitty_local_query(
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
        let bytes = match read_kitty_local(
            control.medium,
            name,
            control.data_size,
            control.data_offset,
            format_f,
            width_s,
            height_v,
        ) {
            Ok(bytes) => bytes,
            Err(_) => {
                if control.quiet >= 2 {
                    return;
                }
                let reply = crate::queries::kitty_probe_einval_reply(control.image_id, "bad data");
                self.state.apply(&TerminalAction::Reply {
                    bytes: reply.into_boxed_slice(),
                });
                return;
            }
        };
        self.answer_kitty_query(format_f, width_s, height_v, &bytes, control);
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
}
