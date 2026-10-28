//! Per-frame peer credentials (`D-07`, "Authorization order" stage 5).
//!
//! On Linux every request frame must carry `SCM_CREDENTIALS` matching the
//! captured peer. The kernel attaches the sending process's credentials to the
//! first byte of each message, so the reader validates credentials on every
//! `recvmsg` and requires them to stay identical across all reads of one frame:
//!
//! - a frame whose first byte comes from a different uid or pid than the
//!   captured peer is `Unauthenticated`;
//! - a credential that changes in the middle of a frame is `Unauthenticated`;
//! - a missing credential is `Unauthenticated`, never "trusted because the
//!   connection was authenticated at accept".
//!
//! The reader never reads past the end of the current frame: it reads the
//! 4-byte header first and then exactly the declared payload. A single
//! `recvmsg` can therefore never span two frames, which is what keeps a live
//! descriptor passed to another process from being attributed to the original
//! peer: the first byte of the injected frame is in the injecting process's own
//! socket buffer and carries its credentials.
//!
//! Cells without a per-message credential mechanism (the non-Linux cells this
//! slice does not serve) fall back to a plain non-blocking read and rely on the
//! captured process re-check before each privileged action. The residual is
//! stated in the crate README; no weaker transport is ever substituted.
//!
//! This is the transport half of the peer check. The service still calls
//! [`crate::peer::PeerGuard::recheck`] after authorization and before every
//! privileged effect.

use std::fmt;
use std::os::unix::net::UnixStream;

use bitty_ipc_api::frame::MAX_FRAME_BYTES;

/// The captured peer a frame must come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FramePeer {
    /// Authenticated uid.
    pub uid: u32,
    /// Captured pid.
    pub pid: u32,
}

/// A frame-read failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// The peer closed the connection.
    Closed,
    /// A credential was missing, foreign, or changed mid-frame.
    Unauthenticated,
    /// The declared payload exceeds the frame bound.
    FrameTooLarge {
        /// The enforced bound.
        limit: usize,
        /// The declared length.
        actual: usize,
    },
    /// The pre-authentication assembly budget would be exceeded by a frame
    /// that has not completed yet.
    BudgetExceeded {
        /// The enforced budget.
        limit: usize,
    },
    /// A socket operation failed.
    Io,
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("peer closed the connection"),
            Self::Unauthenticated => formatter.write_str("frame credential mismatch"),
            Self::FrameTooLarge { limit, actual } => write!(
                formatter,
                "frame declares {actual} bytes, above the {limit}-byte bound"
            ),
            Self::BudgetExceeded { limit } => write!(
                formatter,
                "pre-authentication frame assembly exceeds the {limit}-byte budget"
            ),
            Self::Io => formatter.write_str("socket read failed"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<FrameError> for crate::service::dispatch::Refusal {
    fn from(error: FrameError) -> Self {
        match error {
            FrameError::Unauthenticated => crate::service::dispatch::Refusal::unauthenticated(),
            FrameError::FrameTooLarge { limit, actual } => {
                crate::service::dispatch::Refusal::limit_exceeded(limit as u64, actual as u64)
            }
            FrameError::BudgetExceeded { limit } => {
                crate::service::dispatch::Refusal::limit_exceeded(limit as u64, limit as u64 + 1)
            }
            FrameError::Closed | FrameError::Io => crate::service::dispatch::Refusal::internal(),
        }
    }
}

/// A non-blocking, credential-checking frame reader for one connection.
#[derive(Debug, Default)]
pub struct PeerFrameReader {
    header: Vec<u8>,
    declared: Option<usize>,
    body: Vec<u8>,
    peer: Option<FramePeer>,
}

impl PeerFrameReader {
    /// An empty reader.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes buffered for the frame currently being assembled.
    #[must_use]
    pub fn buffered_len(&self) -> usize {
        self.header.len()
            + self
                .declared
                .map(|declared| self.body.len().min(declared))
                .unwrap_or(0)
    }

    /// Read one complete frame payload.
    ///
    /// Returns `Ok(None)` when no complete frame is ready yet (the state is
    /// retained), and `Err(FrameError::Unauthenticated)` on the first
    /// credential mismatch.
    ///
    /// # Errors
    ///
    /// [`FrameError::Closed`] on EOF, [`FrameError::Unauthenticated`] for a
    /// missing, foreign, or changed credential, [`FrameError::FrameTooLarge`]
    /// for an over-bound declared length, [`FrameError::BudgetExceeded`] when
    /// `cap` is exceeded, and [`FrameError::Io`] for a socket failure.
    pub fn read(
        &mut self,
        stream: &UnixStream,
        expected: FramePeer,
    ) -> Result<Option<Vec<u8>>, FrameError> {
        self.read_capped(stream, expected, None)
    }

    /// Read one complete frame payload with an optional assembly cap.
    ///
    /// `cap` bounds the bytes retained for a frame that has not completed yet,
    /// so the pre-authentication state machine can stop a client that dribbles
    /// a `max_frame_bytes` body before `daemon.negotiate`. The reader never
    /// buffers more than `cap` bytes: a read that would cross it is refused
    /// with [`FrameError::BudgetExceeded`] and the partial frame is dropped. A
    /// frame that completes within the cap is returned normally.
    ///
    /// # Errors
    ///
    /// As [`Self::read`], plus [`FrameError::BudgetExceeded`] when the retained
    /// assembly would exceed `cap`.
    pub fn read_capped(
        &mut self,
        stream: &UnixStream,
        expected: FramePeer,
        cap: Option<usize>,
    ) -> Result<Option<Vec<u8>>, FrameError> {
        loop {
            let want = match self.declared {
                None => 4 - self.header.len(),
                Some(declared) => declared - self.body.len(),
            };
            if want == 0 {
                return Ok(Some(self.finish_frame()));
            }
            let want = match cap {
                Some(cap) => {
                    let buffered = self.buffered_len();
                    if buffered >= cap {
                        self.reset_frame();
                        return Err(FrameError::BudgetExceeded { limit: cap });
                    }
                    // Read at most one byte past the cap so the crossing is
                    // observed without allocating the rest of a declared
                    // `max_frame_bytes` body.
                    want.min(cap - buffered + 1)
                }
                None => want,
            };
            let Some((bytes, credentials)) = read_some(stream, want)? else {
                return Ok(None);
            };
            if !credentials_match(expected, self.peer, credentials) {
                self.reset_frame();
                return Err(FrameError::Unauthenticated);
            }
            if let Some(observed) = credentials {
                self.peer = Some(observed);
            }
            if let Some(cap) = cap {
                if self.buffered_len() + bytes.len() > cap {
                    self.reset_frame();
                    return Err(FrameError::BudgetExceeded { limit: cap });
                }
            }
            if self.declared.is_none() {
                self.header.extend_from_slice(&bytes);
                if self.header.len() == 4 {
                    let declared = u32::from_be_bytes([
                        self.header[0],
                        self.header[1],
                        self.header[2],
                        self.header[3],
                    ]) as usize;
                    if declared > MAX_FRAME_BYTES {
                        self.reset_frame();
                        return Err(FrameError::FrameTooLarge {
                            limit: MAX_FRAME_BYTES,
                            actual: declared,
                        });
                    }
                    self.body = Vec::with_capacity(declared.min(cap.unwrap_or(declared)));
                    self.declared = Some(declared);
                }
            } else {
                self.body.extend_from_slice(&bytes);
            }
        }
    }

    fn reset_frame(&mut self) {
        self.header.clear();
        self.declared = None;
        self.body = Vec::new();
        self.peer = None;
    }

    fn finish_frame(&mut self) -> Vec<u8> {
        let payload = std::mem::take(&mut self.body);
        self.header.clear();
        self.declared = None;
        self.peer = None;
        payload
    }
}

/// Enable `SO_PASSCRED` on a socket.
///
/// On Linux the kernel attaches `SCM_CREDENTIALS` to a message when the
/// **receiving** socket has this option at send time, so the daemon must set it
/// on the accepted socket before the first read. The reader repeats the call
/// defensively (it is idempotent); the accept path calls it first so a client
/// that writes immediately after `connect` is still attributed.
///
/// On cells without the mechanism this is a no-op.
///
/// # Errors
///
/// Returns the socket-option error on Linux.
pub fn enable_credentials(socket: &impl std::os::fd::AsFd) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        nix::sys::socket::setsockopt(socket, nix::sys::socket::sockopt::PassCred, &true)
            .map_err(std::io::Error::from)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = socket;
        Ok(())
    }
}

/// Whether an observed credential matches the captured peer and the earlier
/// reads of the same frame.
///
/// A missing credential is never accepted: the connection was authenticated at
/// accept, but that does not make later unauthenticated bytes trustworthy.
#[must_use]
pub fn credentials_match(
    expected: FramePeer,
    previous: Option<FramePeer>,
    observed: Option<FramePeer>,
) -> bool {
    let Some(observed) = observed else {
        return false;
    };
    if observed.uid != expected.uid || observed.pid != expected.pid {
        return false;
    }
    match previous {
        Some(previous) => previous == observed,
        None => true,
    }
}

type ReadOutcome = Option<(Vec<u8>, Option<FramePeer>)>;

#[cfg(target_os = "linux")]
fn read_some(stream: &UnixStream, want: usize) -> Result<ReadOutcome, FrameError> {
    use std::io::IoSliceMut;
    use std::os::fd::AsRawFd;

    use nix::sys::socket::{
        ControlMessageOwned, MsgFlags, UnixCredentials, recvmsg, setsockopt, sockopt,
    };

    // SO_PASSCRED makes the kernel attach SCM_CREDENTIALS to every received
    // message; it is idempotent and cheap, so the reader owns the guarantee
    // instead of depending on the accept path.
    setsockopt(stream, sockopt::PassCred, &true).map_err(|_| FrameError::Io)?;

    let mut buffer = vec![0u8; want];
    let mut iov = [IoSliceMut::new(&mut buffer)];
    let mut control = nix::cmsg_space!(UnixCredentials);
    let message = match recvmsg::<()>(
        stream.as_raw_fd(),
        &mut iov,
        Some(&mut control),
        MsgFlags::MSG_DONTWAIT,
    ) {
        Ok(message) => message,
        Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EINTR) => return Ok(None),
        Err(_) => return Err(FrameError::Io),
    };
    if message.bytes == 0 {
        return Err(FrameError::Closed);
    }
    let mut credentials = None;
    for control in message.cmsgs().map_err(|_| FrameError::Io)? {
        if let ControlMessageOwned::ScmCredentials(observed) = control {
            credentials = Some(FramePeer {
                uid: observed.uid(),
                pid: u32::try_from(observed.pid()).unwrap_or_default(),
            });
        }
    }
    let bytes = message.bytes;
    let buffer = {
        let mut raw = buffer;
        raw.truncate(bytes);
        raw
    };
    Ok(Some((buffer, credentials)))
}

#[cfg(not(target_os = "linux"))]
fn read_some(stream: &UnixStream, want: usize) -> Result<ReadOutcome, FrameError> {
    use std::os::fd::AsRawFd;

    use nix::sys::socket::{MsgFlags, recv};

    let mut buffer = vec![0u8; want];
    match recv(stream.as_raw_fd(), &mut buffer, MsgFlags::MSG_DONTWAIT) {
        Ok(0) => Err(FrameError::Closed),
        Ok(read) => {
            buffer.truncate(read);
            Ok(Some((buffer, None)))
        }
        Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EINTR) => Ok(None),
        Err(_) => Err(FrameError::Io),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(uid: u32, pid: u32) -> FramePeer {
        FramePeer { uid, pid }
    }

    #[test]
    fn a_matching_credential_is_accepted() {
        assert!(credentials_match(
            peer(1000, 42),
            None,
            Some(peer(1000, 42))
        ));
        assert!(credentials_match(
            peer(1000, 42),
            Some(peer(1000, 42)),
            Some(peer(1000, 42))
        ));
    }

    #[test]
    fn a_foreign_uid_is_unauthenticated() {
        assert!(!credentials_match(
            peer(1000, 42),
            None,
            Some(peer(1001, 42))
        ));
    }

    #[test]
    fn a_foreign_or_recycled_pid_is_unauthenticated() {
        assert!(!credentials_match(
            peer(1000, 42),
            None,
            Some(peer(1000, 43))
        ));
        assert!(!credentials_match(
            peer(1000, 42),
            Some(peer(1000, 42)),
            Some(peer(1000, 43))
        ));
    }

    #[test]
    fn a_missing_credential_is_unauthenticated() {
        assert!(!credentials_match(peer(1000, 42), None, None));
        assert!(!credentials_match(
            peer(1000, 42),
            Some(peer(1000, 42)),
            None
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_partial_frame_is_buffered_across_reads() {
        use std::io::Write as _;

        let (server, mut client) = UnixStream::pair().expect("socketpair");
        enable_credentials(&client).expect("sender credentials");
        server.set_nonblocking(true).expect("nonblocking");
        let mut reader = PeerFrameReader::new();
        let expected = FramePeer {
            uid: nix::unistd::geteuid().as_raw(),
            pid: std::process::id(),
        };
        let payload = b"hello";
        let mut frame = Vec::new();
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        client.write_all(&frame[..3]).expect("partial header");
        assert_eq!(reader.read(&server, expected).expect("no frame yet"), None);
        client.write_all(&frame[3..]).expect("rest");
        let complete = reader.read(&server, expected).expect("frame");
        assert_eq!(complete.as_deref(), Some(payload.as_slice()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_pre_authentication_budget_caps_frame_assembly() {
        use std::io::Write as _;

        let (server, mut client) = UnixStream::pair().expect("socketpair");
        enable_credentials(&client).expect("sender credentials");
        server.set_nonblocking(true).expect("nonblocking");
        let mut reader = PeerFrameReader::new();
        let expected = FramePeer {
            uid: nix::unistd::geteuid().as_raw(),
            pid: std::process::id(),
        };

        // A frame within the cap completes normally.
        let payload = b"ok";
        let mut frame = Vec::new();
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        client.write_all(&frame).expect("small frame");
        let complete = reader
            .read_capped(&server, expected, Some(64))
            .expect("within the budget");
        assert_eq!(complete.as_deref(), Some(payload.as_slice()));

        // A declared `max_frame_bytes` body that dribbles past the cap is
        // refused; the reader never retains more than the cap.
        client
            .write_all(&(MAX_FRAME_BYTES as u32).to_be_bytes())
            .expect("large header");
        client.write_all(&[b'x'; 256]).expect("dribbled body");
        let error = reader
            .read_capped(&server, expected, Some(64))
            .expect_err("budget exceeded");
        assert_eq!(error, FrameError::BudgetExceeded { limit: 64 });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_over_bound_declared_length_is_refused_before_allocation() {
        use std::io::Write as _;

        let (server, mut client) = UnixStream::pair().expect("socketpair");
        enable_credentials(&client).expect("sender credentials");
        server.set_nonblocking(true).expect("nonblocking");
        let mut reader = PeerFrameReader::new();
        let expected = FramePeer {
            uid: nix::unistd::geteuid().as_raw(),
            pid: std::process::id(),
        };
        client
            .write_all(&(MAX_FRAME_BYTES as u32 + 1).to_be_bytes())
            .expect("header");
        let error = reader
            .read(&server, expected)
            .expect_err("over-bound frame");
        assert_eq!(
            error,
            FrameError::FrameTooLarge {
                limit: MAX_FRAME_BYTES,
                actual: MAX_FRAME_BYTES + 1,
            }
        );
    }
}
