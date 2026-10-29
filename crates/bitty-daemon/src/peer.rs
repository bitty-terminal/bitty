//! Peer capture and re-check (`D-07`, `D-08`, `D-15`).
//!
//! Every connection boundary captures the authenticated peer identity before
//! any request byte is parsed, and every privileged dispatch boundary
//! re-validates the captured process. The accepted mechanisms are:
//!
//! - Linux: `SO_PEERCRED` for uid/gid/pid, the peer's `/proc` start time, and a
//!   user-namespace comparison. A uid that only appears to match through a user
//!   namespace or container mapping is `Unauthenticated`, never same-user
//!   success (`D-15`).
//! - macOS: `LOCAL_PEERCRED` for uid/gid and `LOCAL_PEERPID` for the pid. The
//!   safe API surface exposes no process start time in this slice, so the
//!   mechanism is incomplete and [`crate::platform::PlatformCell::serves_endpoint`]
//!   is false. Capture refuses with [`PeerError::UnsupportedPlatform`]; the
//!   code is compiled and cross-checked but no macOS build can serve an
//!   endpoint or construct a guard until a start-time mechanism lands.
//! - Every other cell: [`PeerError::UnsupportedPlatform`]. A weaker check is
//!   never substituted. FreeBSD has `LOCAL_PEERCRED` (uid/gid) but no pid in
//!   the safe API, so it fails the same gate.
//!
//! Re-check failures fail closed: a dead peer, a recycled pid, a changed start
//! time, a changed uid, a changed user namespace, or an unavailable
//! credential all refuse the dispatch and apply nothing. The service loop
//! (bitty#2607) maps a failure to `Unauthenticated`, closes the connection,
//! and records the boundary in audit.
//!
//! The pid alone is informational (`D-08`) because it can be reused; the
//! start time is what makes the check meaningful. On Linux the capture reads
//! `/proc/<pid>/stat` once for the start time and `/proc/<pid>/status` for the
//! effective uid, both with a fixed byte bound and a fail-closed parse.
//!
//! # Known caveats (recorded follow-ups)
//!
//! - The start time is a per-boot tick, not a boot id. Re-checking happens
//!   inside one daemon lifetime, so a reboot ends the daemon before the value
//!   could be compared across boots; recording a boot id and holding a pidfd
//!   (Linux `SO_PEERPIDFD`) would harden the capture against a pid reused
//!   between `SO_PEERCRED` and the `/proc` reads.
//! - The `/proc` evidence is a mixed snapshot: the start time, effective uid,
//!   and user namespace are separate reads, so a peer that exits and has its
//!   pid reused between them can yield fields from two processes. The
//!   start-time comparison at every re-check refuses the recycled pid after
//!   the fact, but the capture observation is not atomic; a pidfd would close
//!   that window.

use std::fmt;

#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(unix)]
use std::os::unix::net::UnixStream;

#[cfg(unix)]
use nix::sys::socket::{getsockopt, sockopt};
#[cfg(unix)]
use nix::unistd::geteuid;

#[cfg(unix)]
use crate::platform::PeerMechanism;
use crate::platform::PlatformCell;

/// A captured, authenticated peer identity.
///
/// The start time is platform-dependent: `Some` where the platform exposes a
/// process start time through the accepted mechanism, `None` where it does
/// not. The raw value is a monotonic per-boot process start tick, not a
/// timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerIdentity {
    uid: u32,
    gid: u32,
    pid: u32,
    start_time: Option<u64>,
}

impl PeerIdentity {
    /// The peer's authenticated uid.
    #[must_use]
    pub const fn uid(&self) -> u32 {
        self.uid
    }

    /// The peer's captured gid.
    #[must_use]
    pub const fn gid(&self) -> u32 {
        self.gid
    }

    /// The peer's pid. Informational: a pid can be reused (`D-08`).
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// The peer's process start time, where the platform exposes one.
    #[must_use]
    pub const fn start_time(&self) -> Option<u64> {
        self.start_time
    }
}

/// A method boundary that requires a peer re-check before it is applied.
///
/// This is the closed set named by the contract's "Stale handles, peer
/// re-check, and refusals" section: create, controller attach, abort, kill,
/// transfer, detach-other, grant, and stop. A future privileged method must add
/// its own variant here so the service loop's exhaustive match cannot silently
/// skip the check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DispatchBoundary {
    /// `session.create`.
    CreateSession,
    /// `session.attach` with the controller role.
    ControllerAttach,
    /// `session.abort` with a consumer reason.
    Abort,
    /// `session.kill` (operator only).
    Kill,
    /// `session.control.transfer`.
    ControlTransfer,
    /// Detach or revoke of another lease.
    DetachOther,
    /// `session.grant` (observer-grant mint).
    ObserverGrantMint,
    /// `session.grant.revoke` (observer-grant revocation).
    ObserverGrantRevoke,
    /// `daemon.host_grant` mint (host-consent grant).
    HostGrantMint,
    /// `daemon.host_grant` revoke (host-consent grant).
    HostGrantRevoke,
    /// `daemon.stop` in any mode.
    DaemonStop,
    /// `daemon.orphans.verify`.
    OrphanVerify,
    /// Controller-only `session.resize`.
    Resize,
    /// Controller-only terminal content (`session.read`, `session.write`,
    /// `session.subscribe`).
    ContentPlane,
}

impl DispatchBoundary {
    /// Every privileged boundary in contract order.
    pub const ALL: [Self; 14] = [
        Self::CreateSession,
        Self::ControllerAttach,
        Self::Abort,
        Self::Kill,
        Self::ControlTransfer,
        Self::DetachOther,
        Self::ObserverGrantMint,
        Self::ObserverGrantRevoke,
        Self::HostGrantMint,
        Self::HostGrantRevoke,
        Self::DaemonStop,
        Self::OrphanVerify,
        Self::Resize,
        Self::ContentPlane,
    ];

    /// The method this boundary is reported as in diagnostics.
    ///
    /// Most boundaries guard exactly one contract method; [`Self::ContentPlane`]
    /// guards `session.read`, `session.write`, and `session.subscribe`, so it
    /// reports the synthetic `session.content` label.
    #[must_use]
    pub const fn method(self) -> &'static str {
        match self {
            Self::CreateSession => "session.create",
            Self::ControllerAttach => "session.attach",
            Self::Abort => "session.abort",
            Self::Kill => "session.kill",
            Self::ControlTransfer => "session.control.transfer",
            Self::DetachOther => "session.detach",
            Self::ObserverGrantMint => "session.grant",
            Self::ObserverGrantRevoke => "session.grant.revoke",
            Self::HostGrantMint | Self::HostGrantRevoke => "daemon.host_grant",
            Self::DaemonStop => "daemon.stop",
            Self::OrphanVerify => "daemon.orphans.verify",
            Self::Resize => "session.resize",
            Self::ContentPlane => "session.content",
        }
    }
}

impl fmt::Display for DispatchBoundary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.method())
    }
}

/// Why a peer capture or re-check failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerCause {
    /// The credential could not be captured or inspected; fail closed.
    Unavailable {
        /// What was unavailable.
        reason: String,
    },
    /// The peer is not this user, or its identity changed underneath us.
    Unauthenticated {
        /// Why the identity does not match.
        reason: String,
    },
    /// The peer process is gone.
    Exited,
    /// The pid still exists but its start time changed: the pid was recycled.
    Changed {
        /// The start time captured at the handshake.
        expected: u64,
        /// The start time observed at the dispatch boundary.
        actual: u64,
    },
}

impl fmt::Display for PeerCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { reason } => {
                write!(formatter, "peer credential unavailable: {reason}")
            }
            Self::Unauthenticated { reason } => {
                write!(formatter, "peer identity is not authenticated: {reason}")
            }
            Self::Exited => formatter.write_str("peer process exited"),
            Self::Changed { expected, actual } => write!(
                formatter,
                "peer start time changed from {expected} to {actual}; the pid was recycled"
            ),
        }
    }
}

/// A typed peer failure.
///
/// [`PeerError::UnsupportedPlatform`] maps to `Denied/UnsupportedPlatform`;
/// every capture or re-check failure maps to `Unauthenticated` and closes the
/// connection without applying the dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerError {
    /// The cell has no accepted peer-identity mechanism.
    UnsupportedPlatform {
        /// The refused cell.
        cell: PlatformCell,
    },
    /// The handshake capture failed; the connection must be refused.
    CaptureFailed {
        /// The failure.
        cause: PeerCause,
    },
    /// A privileged dispatch boundary failed the re-check.
    RecheckFailed {
        /// The boundary that was refused.
        boundary: DispatchBoundary,
        /// The failure.
        cause: PeerCause,
    },
}

impl PeerError {
    /// The static contract result code for this failure.
    #[must_use]
    pub const fn result_code(&self) -> &'static str {
        match self {
            Self::UnsupportedPlatform { .. } => "UnsupportedPlatform",
            Self::CaptureFailed { .. } | Self::RecheckFailed { .. } => "Unauthenticated",
        }
    }
}

impl fmt::Display for PeerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform { cell } => write!(
                formatter,
                "no accepted peer-identity mechanism on {cell}; refusing instead of \
                 accepting a weaker check"
            ),
            Self::CaptureFailed { cause } => write!(formatter, "peer capture failed: {cause}"),
            Self::RecheckFailed { boundary, cause } => {
                write!(formatter, "peer re-check failed at {boundary}: {cause}")
            }
        }
    }
}

impl std::error::Error for PeerError {}

/// A captured peer identity bound to the process that must still own it.
///
/// Construct it once at the handshake with [`PeerGuard::capture`], then call
/// [`PeerGuard::recheck`] before applying every privileged dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerGuard {
    identity: PeerIdentity,
    expected_uid: u32,
}

impl PeerGuard {
    /// Capture and authenticate the connected peer before parsing any request.
    ///
    /// The argument is the accepted, connected [`UnixStream`], not a generic
    /// descriptor: `SO_PEERCRED` on a *listening* socket reports the daemon's
    /// own pid/uid/gid, so an `AsFd` API would let a caller "authenticate" the
    /// listener as its own peer. The expect-uid is read from the process with
    /// `geteuid()`; it is never a caller-supplied value. An
    /// `SO_ACCEPTCONN` check refuses a listening descriptor as belt-and-braces
    /// for any future raw-descriptor path.
    ///
    /// Windows has no capture path until the named-pipe transport lands; the
    /// endpoint refuses that cell before a connection exists.
    ///
    /// # Errors
    ///
    /// [`PeerError::UnsupportedPlatform`] when the compiled cell is not
    /// servable (no accepted transport, or a peer mechanism without a start
    /// time), and [`PeerError::CaptureFailed`] when the kernel credential is
    /// unavailable, the descriptor is a listening socket or not a socket, the
    /// peer is not the daemon uid, the peer is already gone, or the identity
    /// cannot be bounded and verified.
    #[cfg(unix)]
    pub fn capture(stream: &UnixStream) -> Result<Self, PeerError> {
        Self::capture_inner(stream, geteuid().as_raw())
    }

    /// The captured peer identity, for audit and diagnostics.
    #[must_use]
    pub const fn identity(&self) -> PeerIdentity {
        self.identity
    }

    /// Re-validate the captured peer before a privileged dispatch.
    ///
    /// # Errors
    ///
    /// [`PeerError::RecheckFailed`] when the peer exited, its pid was recycled
    /// with a different start time, its uid or user namespace no longer
    /// matches, or the identity cannot be inspected. Nothing is applied on a
    /// refusal.
    pub fn recheck(&self, boundary: DispatchBoundary) -> Result<(), PeerError> {
        self.recheck_with(&PlatformProbe, boundary)
    }

    /// A guard bound to the current test process.
    ///
    /// Test-only: it captures `SO_PEERCRED` from a socketpair, which reports
    /// the current process, so `recheck` exercises the real `/proc` path and
    /// succeeds for every boundary while the test process is alive.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn for_current_process() -> Self {
        let (stream, other) = UnixStream::pair().expect("socketpair");
        let guard = Self::capture(&stream).expect("capture self");
        drop(other);
        guard
    }

    /// A guard whose captured start time is forged.
    ///
    /// Test-only: a live pid with a start time that does not match `/proc` is
    /// exactly the recycled-pid condition `recheck` must refuse.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn with_forged_start_time(pid: u32, start_time: u64) -> Self {
        let uid = geteuid().as_raw();
        Self {
            identity: PeerIdentity {
                uid,
                gid: nix::unistd::getegid().as_raw(),
                pid,
                start_time: Some(start_time),
            },
            expected_uid: uid,
        }
    }

    #[cfg(unix)]
    fn capture_inner<S: AsFd>(socket: S, expected_uid: u32) -> Result<Self, PeerError> {
        Self::capture_for_cell(PlatformCell::current(), socket, expected_uid)
    }

    #[cfg(unix)]
    fn capture_for_cell<S: AsFd>(
        cell: PlatformCell,
        socket: S,
        expected_uid: u32,
    ) -> Result<Self, PeerError> {
        if !cell.serves_endpoint() {
            return Err(PeerError::UnsupportedPlatform { cell });
        }
        let listening =
            getsockopt(&socket, sockopt::AcceptConn).map_err(|error| PeerError::CaptureFailed {
                cause: PeerCause::Unavailable {
                    reason: format!("SO_ACCEPTCONN query failed: {error}"),
                },
            })?;
        if listening {
            return Err(PeerError::CaptureFailed {
                cause: PeerCause::Unauthenticated {
                    reason: concat!(
                        "a listening socket has no connected peer; ",
                        "its SO_PEERCRED is the daemon's own identity"
                    )
                    .to_string(),
                },
            });
        }
        let identity = capture_identity(cell.peer_mechanism(), socket, expected_uid)
            .map_err(|cause| PeerError::CaptureFailed { cause })?;
        Ok(Self {
            identity,
            expected_uid,
        })
    }

    fn recheck_with<P: ProcessProbe>(
        &self,
        probe: &P,
        boundary: DispatchBoundary,
    ) -> Result<(), PeerError> {
        if self.identity.uid != self.expected_uid {
            return Err(PeerError::RecheckFailed {
                boundary,
                cause: PeerCause::Unauthenticated {
                    reason: format!(
                        "captured uid {} is not the daemon uid {}",
                        self.identity.uid, self.expected_uid
                    ),
                },
            });
        }
        let status = probe
            .probe(self.identity.pid)
            .map_err(|cause| PeerError::RecheckFailed { boundary, cause })?;
        match status {
            ProcessStatus::Gone => Err(PeerError::RecheckFailed {
                boundary,
                cause: PeerCause::Exited,
            }),
            ProcessStatus::Alive { uid, start_time } => {
                if let Some(uid) = uid {
                    if uid != self.identity.uid {
                        return Err(PeerError::RecheckFailed {
                            boundary,
                            cause: PeerCause::Unauthenticated {
                                reason: format!(
                                    "peer uid changed from {} to {uid}",
                                    self.identity.uid
                                ),
                            },
                        });
                    }
                }
                match (self.identity.start_time, start_time) {
                    (Some(expected), Some(actual)) if expected != actual => {
                        Err(PeerError::RecheckFailed {
                            boundary,
                            cause: PeerCause::Changed { expected, actual },
                        })
                    }
                    (Some(_), None) => Err(PeerError::RecheckFailed {
                        boundary,
                        cause: PeerCause::Unavailable {
                            reason: "the captured start time can no longer be read".to_string(),
                        },
                    }),
                    _ => Ok(()),
                }
            }
        }
    }
}

/// One observation of a peer process.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos")),
    allow(dead_code, reason = "no captured peer can exist on this cell")
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessStatus {
    /// The process no longer exists.
    Gone,
    /// The process exists.
    Alive {
        /// Start time, where the platform exposes it.
        start_time: Option<u64>,
        /// Effective uid, where the platform exposes it.
        uid: Option<u32>,
    },
}

trait ProcessProbe {
    fn probe(&self, pid: u32) -> Result<ProcessStatus, PeerCause>;
}

struct PlatformProbe;

impl ProcessProbe for PlatformProbe {
    fn probe(&self, pid: u32) -> Result<ProcessStatus, PeerCause> {
        platform_probe(pid)
    }
}

#[cfg(target_os = "linux")]
fn platform_probe(pid: u32) -> Result<ProcessStatus, PeerCause> {
    linux::probe(pid)
}

#[cfg(target_os = "macos")]
fn platform_probe(pid: u32) -> Result<ProcessStatus, PeerCause> {
    apple::probe(pid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_probe(_pid: u32) -> Result<ProcessStatus, PeerCause> {
    Err(PeerCause::Unavailable {
        reason: "no accepted peer process probe on this platform".to_string(),
    })
}

#[cfg(unix)]
fn capture_identity<S: AsFd>(
    mechanism: PeerMechanism,
    socket: S,
    expected_uid: u32,
) -> Result<PeerIdentity, PeerCause> {
    match mechanism {
        #[cfg(target_os = "linux")]
        PeerMechanism::LinuxPeerCred => linux::capture(socket, expected_uid),
        #[cfg(target_os = "macos")]
        PeerMechanism::AppleLocalPeer => apple::capture(socket, expected_uid),
        _ => Err(PeerCause::Unavailable {
            reason: format!("peer mechanism {mechanism:?} is unavailable on this build"),
        }),
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;

    use nix::sys::socket::{getsockopt, sockopt};

    use super::{PeerCause, PeerIdentity, ProcessStatus};

    /// Bounded read for `/proc/<pid>/stat`; a real line is far below this.
    const PROC_STAT_MAX_BYTES: u64 = 4 * 1024;

    /// Bounded read for `/proc/<pid>/status`; only the `Uid:` line is used.
    const PROC_STATUS_MAX_BYTES: u64 = 8 * 1024;

    pub(super) fn capture<S: AsFd>(
        socket: S,
        expected_uid: u32,
    ) -> Result<PeerIdentity, PeerCause> {
        let credentials = getsockopt(&socket, sockopt::PeerCredentials).map_err(|err| {
            PeerCause::Unavailable {
                reason: format!("SO_PEERCRED query failed: {err}"),
            }
        })?;
        let raw_pid = credentials.pid();
        if raw_pid <= 0 {
            return Err(PeerCause::Unavailable {
                reason: "SO_PEERCRED returned no peer pid".to_string(),
            });
        }
        let pid = u32::try_from(raw_pid).map_err(|_| PeerCause::Unavailable {
            reason: "SO_PEERCRED returned a negative peer pid".to_string(),
        })?;
        let uid = credentials.uid();
        if uid != expected_uid {
            return Err(PeerCause::Unauthenticated {
                reason: format!("peer uid {uid} is not the daemon uid {expected_uid}"),
            });
        }
        let status = probe(pid)?;
        let (start_time, process_uid) = match status {
            ProcessStatus::Gone => return Err(PeerCause::Exited),
            ProcessStatus::Alive {
                start_time: Some(start_time),
                uid: Some(uid),
            } => (start_time, uid),
            ProcessStatus::Alive { .. } => {
                return Err(PeerCause::Unavailable {
                    reason: "peer start time or uid is unavailable from /proc".to_string(),
                });
            }
        };
        if process_uid != uid {
            return Err(PeerCause::Unauthenticated {
                reason: format!(
                    "SO_PEERCRED uid {uid} does not match the peer process uid {process_uid}"
                ),
            });
        }
        Ok(PeerIdentity {
            uid,
            gid: credentials.gid(),
            pid,
            start_time: Some(start_time),
        })
    }

    pub(super) fn probe(pid: u32) -> Result<ProcessStatus, PeerCause> {
        let Some(start_time) = read_start_time(pid)? else {
            return Ok(ProcessStatus::Gone);
        };
        let Some(uid) = read_effective_uid(pid)? else {
            return Ok(ProcessStatus::Gone);
        };
        if !same_user_namespace(pid)? {
            return Err(PeerCause::Unauthenticated {
                reason: format!(
                    "peer pid {pid} is in a different user namespace; a mapped uid is not \
                     same-user success"
                ),
            });
        }
        Ok(ProcessStatus::Alive {
            start_time: Some(start_time),
            uid: Some(uid),
        })
    }

    fn read_start_time(pid: u32) -> Result<Option<u64>, PeerCause> {
        let path = format!("/proc/{pid}/stat");
        let Some(file) = open_proc(&path)? else {
            return Ok(None);
        };
        let bytes = read_capped(file, PROC_STAT_MAX_BYTES, &path)?;
        let close = bytes
            .iter()
            .rposition(|byte| *byte == b')')
            .ok_or_else(|| PeerCause::Unavailable {
                reason: format!("{path} has no process-name terminator"),
            })?;
        // Field 3 is the state, so field 22 (starttime) is index 19.
        let field = bytes[close + 1..]
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty())
            .nth(19)
            .ok_or_else(|| PeerCause::Unavailable {
                reason: format!("{path} has no starttime field"),
            })?;
        parse_u64(field, &path, "starttime").map(Some)
    }

    fn read_effective_uid(pid: u32) -> Result<Option<u32>, PeerCause> {
        let path = format!("/proc/{pid}/status");
        let Some(file) = open_proc(&path)? else {
            return Ok(None);
        };
        let bytes = read_capped(file, PROC_STATUS_MAX_BYTES, &path)?;
        let line = bytes
            .split(|byte| *byte == b'\n')
            .find_map(|line| line.strip_prefix(b"Uid:"))
            .ok_or_else(|| PeerCause::Unavailable {
                reason: format!("{path} has no Uid field"),
            })?;
        let field = line
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty())
            .nth(1)
            .ok_or_else(|| PeerCause::Unavailable {
                reason: format!("{path} has no effective uid"),
            })?;
        let text = std::str::from_utf8(field).map_err(|_| PeerCause::Unavailable {
            reason: format!("{path} effective uid is not ASCII"),
        })?;
        text.parse::<u32>()
            .map(Some)
            .map_err(|_| PeerCause::Unavailable {
                reason: format!("{path} effective uid is not an unsigned integer"),
            })
    }

    /// Read at most `bound` bytes. A truncated stat line fails at the field
    /// parse; a truncated status file is fine because the `Uid:` line comes
    /// before the variable-length `Groups:` line.
    fn read_capped(mut file: File, bound: u64, path: &str) -> Result<Vec<u8>, PeerCause> {
        let mut bytes = Vec::new();
        file.by_ref()
            .take(bound)
            .read_to_end(&mut bytes)
            .map_err(|err| unavailable(path, &err))?;
        Ok(bytes)
    }

    fn parse_u64(field: &[u8], path: &str, name: &str) -> Result<u64, PeerCause> {
        let text = std::str::from_utf8(field).map_err(|_| PeerCause::Unavailable {
            reason: format!("{path} {name} is not ASCII"),
        })?;
        text.parse::<u64>().map_err(|_| PeerCause::Unavailable {
            reason: format!("{path} {name} is not an unsigned integer"),
        })
    }

    fn open_proc(path: &str) -> Result<Option<File>, PeerCause> {
        match File::open(path) {
            Ok(file) => Ok(Some(file)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(unavailable(path, &err)),
        }
    }

    fn unavailable(path: &str, err: &std::io::Error) -> PeerCause {
        PeerCause::Unavailable {
            reason: format!("{path} cannot be read: {err}"),
        }
    }

    fn same_user_namespace(pid: u32) -> Result<bool, PeerCause> {
        let own =
            std::fs::metadata("/proc/self/ns/user").map_err(|err| PeerCause::Unavailable {
                reason: format!("/proc/self/ns/user cannot be read: {err}"),
            })?;
        let peer_path = format!("/proc/{pid}/ns/user");
        let peer = match std::fs::metadata(&peer_path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(unavailable(&peer_path, &err)),
        };
        Ok(own.dev() == peer.dev() && own.ino() == peer.ino())
    }
}

#[cfg(target_os = "macos")]
mod apple {
    use std::os::fd::AsFd;

    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::sys::socket::{getsockopt, sockopt};
    use nix::unistd::Pid;

    use super::{PeerCause, PeerIdentity, ProcessStatus};

    pub(super) fn capture<S: AsFd>(
        socket: S,
        expected_uid: u32,
    ) -> Result<PeerIdentity, PeerCause> {
        let credentials =
            getsockopt(&socket, sockopt::LocalPeerCred).map_err(|err| PeerCause::Unavailable {
                reason: format!("LOCAL_PEERCRED query failed: {err}"),
            })?;
        let raw_pid =
            getsockopt(&socket, sockopt::LocalPeerPid).map_err(|err| PeerCause::Unavailable {
                reason: format!("LOCAL_PEERPID query failed: {err}"),
            })?;
        if raw_pid <= 0 {
            return Err(PeerCause::Unavailable {
                reason: "LOCAL_PEERPID returned no peer pid".to_string(),
            });
        }
        let pid = u32::try_from(raw_pid).map_err(|_| PeerCause::Unavailable {
            reason: "LOCAL_PEERPID returned a negative peer pid".to_string(),
        })?;
        let uid = credentials.uid();
        if uid != expected_uid {
            return Err(PeerCause::Unauthenticated {
                reason: format!("peer uid {uid} is not the daemon uid {expected_uid}"),
            });
        }
        // `LOCAL_PEERCRED` has no separate gid; the first group is the
        // effective gid.
        let gid = credentials.groups().first().copied().unwrap_or_default();
        Ok(PeerIdentity {
            uid,
            gid,
            pid,
            // The safe API surface in this slice has no start-time call; the
            // cell is conformance-gated until one lands.
            start_time: None,
        })
    }

    pub(super) fn probe(pid: u32) -> Result<ProcessStatus, PeerCause> {
        let Ok(raw_pid) = i32::try_from(pid) else {
            return Err(PeerCause::Unavailable {
                reason: format!("peer pid {pid} does not fit the platform pid type"),
            });
        };
        match kill(Pid::from_raw(raw_pid), None::<nix::sys::signal::Signal>) {
            Ok(()) => Ok(ProcessStatus::Alive {
                start_time: None,
                uid: None,
            }),
            Err(Errno::ESRCH) => Ok(ProcessStatus::Gone),
            Err(Errno::EPERM) => Err(PeerCause::Unauthenticated {
                reason: format!("peer pid {pid} exists but is not signalable by this user"),
            }),
            Err(err) => Err(PeerCause::Unavailable {
                reason: format!("peer liveness check failed: {err}"),
            }),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
// The peer guard tests capture the real /proc start time; the served loop is
// Linux-only (D-15), so they do not run on the other tested cells.
mod tests {
    use super::*;

    #[test]
    fn dispatch_boundaries_are_the_contract_methods() {
        assert_eq!(
            DispatchBoundary::ALL.len(),
            14,
            "the contract boundaries, the two host-grant modes, and the content plane"
        );
        assert_eq!(DispatchBoundary::CreateSession.method(), "session.create");
        assert_eq!(
            DispatchBoundary::ControlTransfer.method(),
            "session.control.transfer"
        );
        assert_eq!(
            DispatchBoundary::ObserverGrantRevoke.method(),
            "session.grant.revoke"
        );
        assert_eq!(
            DispatchBoundary::HostGrantMint.method(),
            DispatchBoundary::HostGrantRevoke.method()
        );
        assert_eq!(
            DispatchBoundary::HostGrantMint.method(),
            "daemon.host_grant"
        );
        assert_eq!(DispatchBoundary::DaemonStop.to_string(), "daemon.stop");
        for boundary in DispatchBoundary::ALL {
            assert!(boundary.method().contains('.'));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_recycled_pid_is_refused_by_the_start_time() {
        let real = PeerGuard::for_current_process();
        let start_time = real
            .identity()
            .start_time()
            .expect("Linux exposes a process start time");
        let forged =
            PeerGuard::with_forged_start_time(std::process::id(), start_time.saturating_add(1));
        let error = forged
            .recheck(DispatchBoundary::CreateSession)
            .expect_err("a pid with a different start time is a recycled pid");
        assert!(matches!(
            error,
            PeerError::RecheckFailed {
                boundary: DispatchBoundary::CreateSession,
                cause: PeerCause::Changed { .. }
            }
        ));
        real.recheck(DispatchBoundary::CreateSession)
            .expect("the real process still matches");
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_platform_is_a_typed_refusal() {
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let error = PeerGuard::capture_for_cell(PlatformCell::FreeBsdX86_64, &socket, 0)
            .expect_err("FreeBSD has no accepted peer mechanism in this slice");
        assert!(matches!(
            error,
            PeerError::UnsupportedPlatform {
                cell: PlatformCell::FreeBsdX86_64
            }
        ));
        assert_eq!(error.result_code(), "UnsupportedPlatform");
    }

    #[cfg(unix)]
    #[test]
    fn macos_is_refused_structurally_not_by_prose() {
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let uid = geteuid().as_raw();
        let error = PeerGuard::capture_for_cell(PlatformCell::MacosArm64, &socket, uid)
            .expect_err("macOS has no start-time mechanism, so it must not capture");
        assert!(matches!(
            error,
            PeerError::UnsupportedPlatform {
                cell: PlatformCell::MacosArm64
            }
        ));
        assert_eq!(error.result_code(), "UnsupportedPlatform");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn capture_refuses_a_listening_socket() {
        let dir = scratch_dir("listener");
        let path = dir.join("listener.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind a listener");
        // `SO_PEERCRED` on a listening socket reports the daemon's own
        // credentials; the structural signature and `SO_ACCEPTCONN` must
        // refuse before that value can be mistaken for a peer.
        let error = PeerGuard::capture_inner(&listener, geteuid().as_raw())
            .expect_err("a listening socket has no peer to authenticate");
        assert!(matches!(
            error,
            PeerError::CaptureFailed {
                cause: PeerCause::Unauthenticated { .. }
            }
        ));
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn capture_refuses_a_non_socket_descriptor() {
        let dir = scratch_dir("plain-file");
        let path = dir.join("plain.txt");
        std::fs::write(&path, b"not a socket").expect("write the file");
        let file = std::fs::File::open(&path).expect("open the file");
        let error = PeerGuard::capture_inner(&file, geteuid().as_raw())
            .expect_err("a regular file has no peer credentials");
        assert!(matches!(
            error,
            PeerError::CaptureFailed {
                cause: PeerCause::Unavailable { .. }
            }
        ));
        drop(file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    fn scratch_dir(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};

        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "bittyd-peer-unit-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create scratch dir");
        path
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn capture_matches_the_current_process_identity() {
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        let pid = u32::try_from(nix::unistd::getpid().as_raw()).expect("pid fits u32");
        let guard = PeerGuard::capture(&socket).expect("capture the peer");
        let identity = guard.identity();
        assert_eq!(identity.uid(), uid);
        assert_eq!(identity.gid(), gid);
        assert_eq!(identity.pid(), pid);
        let ProcessStatus::Alive {
            start_time: Some(expected),
            ..
        } = linux::probe(pid).expect("probe self")
        else {
            panic!("current process must be alive with a start time");
        };
        assert_eq!(identity.start_time(), Some(expected));
        for boundary in DispatchBoundary::ALL {
            guard
                .recheck(boundary)
                .expect("the peer is still this process");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn capture_refuses_a_foreign_uid() {
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let uid = geteuid().as_raw();
        let other = uid.wrapping_add(1);
        let error = PeerGuard::capture_inner(&socket, other).expect_err("uid mismatch must refuse");
        assert!(matches!(
            error,
            PeerError::CaptureFailed {
                cause: PeerCause::Unauthenticated { .. }
            }
        ));
        assert_eq!(error.result_code(), "Unauthenticated");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recheck_refuses_a_changed_start_time() {
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let guard = PeerGuard::capture(&socket).expect("capture the peer");
        let expected = guard.identity().start_time().expect("Linux start time");

        struct DriftProbe(u64);
        impl ProcessProbe for DriftProbe {
            fn probe(&self, _pid: u32) -> Result<ProcessStatus, PeerCause> {
                Ok(ProcessStatus::Alive {
                    start_time: Some(self.0),
                    uid: Some(nix::unistd::getuid().as_raw()),
                })
            }
        }

        let error = guard
            .recheck_with(&DriftProbe(expected + 1), DispatchBoundary::Kill)
            .expect_err("a recycled pid must be refused");
        assert!(matches!(
            error,
            PeerError::RecheckFailed {
                boundary: DispatchBoundary::Kill,
                cause: PeerCause::Changed { expected: at_capture, actual } ,
            } if at_capture == expected && actual == expected + 1
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recheck_refuses_a_gone_peer() {
        struct GoneProbe;
        impl ProcessProbe for GoneProbe {
            fn probe(&self, _pid: u32) -> Result<ProcessStatus, PeerCause> {
                Ok(ProcessStatus::Gone)
            }
        }

        let (socket, _peer) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let guard = PeerGuard::capture(&socket).expect("capture the peer");
        let error = guard
            .recheck_with(&GoneProbe, DispatchBoundary::Abort)
            .expect_err("a dead peer must be refused");
        assert!(matches!(
            error,
            PeerError::RecheckFailed {
                boundary: DispatchBoundary::Abort,
                cause: PeerCause::Exited,
            }
        ));
    }
}
