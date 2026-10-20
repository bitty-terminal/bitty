//! Bounded one-record framing for the Core publication-control driver.
//!
//! One control frame is exactly a four-byte big-endian `u32` length followed
//! by that many record bytes. The prefix is read into a stack buffer, checked
//! against the named contract bounds, and only then is the body allocated.
//! A zero-length, oversized, short, truncated, or trailing-byte frame fails
//! closed before the driver can act on it.
//!
//! Each call moves exactly one record. There is no queue here: the caller owns
//! its bounded queue and every deadline, and this module creates no thread,
//! socket, listener, or clock.

use std::fmt;
use std::io::{self, Read, Write};

use bitty_ipc_api::provider_publication::{
    CONTROL_FRAME_MAX_BYTES, CONTROL_FRAME_PREFIX_BYTES, CONTROL_HEADER_BYTES, ControlError,
    ControlRecord,
};

/// Fail-closed framed-I/O rejection.
///
/// No variant carries a payload byte, record identifier, path, or secret.
#[derive(Debug)]
pub enum ControlIoError {
    /// The stream closed cleanly between frames.
    Closed,
    /// The stream ended part-way through the four-byte length prefix.
    TruncatedPrefix {
        /// Prefix bytes received before EOF.
        received: usize,
    },
    /// The stream ended part-way through the declared record body.
    TruncatedBody {
        /// Declared body length.
        expected: usize,
        /// Body bytes received before EOF.
        received: usize,
    },
    /// The declared frame length is zero.
    ZeroLength,
    /// The declared frame length exceeds [`CONTROL_FRAME_MAX_BYTES`].
    FrameTooLarge,
    /// The declared frame is shorter than a control record header.
    FrameShorterThanHeader {
        /// Declared length.
        actual: usize,
    },
    /// The bounded body allocation failed.
    AllocationFailed {
        /// Requested body bytes.
        requested: usize,
    },
    /// The complete frame is not a valid control record.
    Codec(ControlError),
    /// The underlying stream failed.
    Io(io::Error),
    /// A write made no progress.
    WriteZero,
    /// This platform has no authenticated publication-control transport.
    UnsupportedPlatform,
}

impl fmt::Display for ControlIoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("control stream closed between frames"),
            Self::TruncatedPrefix { .. } => {
                formatter.write_str("control stream ended inside a frame length prefix")
            }
            Self::TruncatedBody { .. } => {
                formatter.write_str("control stream ended inside a control record")
            }
            Self::ZeroLength => formatter.write_str("control frame length is zero"),
            Self::FrameTooLarge => formatter.write_str("control frame exceeds the named bound"),
            Self::FrameShorterThanHeader { .. } => {
                formatter.write_str("control frame is shorter than a record header")
            }
            Self::AllocationFailed { .. } => formatter.write_str("control frame allocation failed"),
            Self::Codec(_) => formatter.write_str("control record rejected"),
            Self::Io(_) => formatter.write_str("control stream I/O failed"),
            Self::WriteZero => formatter.write_str("control stream write made no progress"),
            Self::UnsupportedPlatform => {
                formatter.write_str("publication control is unsupported on this platform")
            }
        }
    }
}

impl std::error::Error for ControlIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Codec(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Closed
            | Self::TruncatedPrefix { .. }
            | Self::TruncatedBody { .. }
            | Self::ZeroLength
            | Self::FrameTooLarge
            | Self::FrameShorterThanHeader { .. }
            | Self::AllocationFailed { .. }
            | Self::WriteZero
            | Self::UnsupportedPlatform => None,
        }
    }
}

/// Record-oriented I/O over one authenticated control stream.
///
/// Implementations send or receive exactly one bounded, length-prefixed
/// [`ControlRecord`] per call. They never queue, retry, or wait past the
/// caller's own deadline.
pub trait ControlRecordIo {
    /// Send exactly one complete control record.
    ///
    /// # Errors
    ///
    /// Returns [`ControlIoError`] on framing, codec, or stream failure.
    fn send(&mut self, record: &ControlRecord) -> Result<(), ControlIoError>;

    /// Receive exactly one complete control record.
    ///
    /// # Errors
    ///
    /// Returns [`ControlIoError`] on framing, codec, or stream failure.
    fn receive(&mut self) -> Result<ControlRecord, ControlIoError>;
}

/// Write one length-prefixed control record.
// Used by the Unix authenticated transport and by the cross-platform
// state-machine tests. On non-Unix the shipped transport fails closed before
// framing, so the non-test lib has no caller.
#[cfg_attr(not(unix), allow(dead_code))]
pub(super) fn send_record<W: Write + ?Sized>(
    writer: &mut W,
    record: &ControlRecord,
) -> Result<(), ControlIoError> {
    let frame = record.encode().map_err(ControlIoError::Codec)?;
    if frame.is_empty() {
        return Err(ControlIoError::ZeroLength);
    }
    if frame.len() > CONTROL_FRAME_MAX_BYTES {
        return Err(ControlIoError::FrameTooLarge);
    }
    let length = u32::try_from(frame.len()).map_err(|_| ControlIoError::FrameTooLarge)?;
    write_all(writer, &length.to_be_bytes())?;
    write_all(writer, &frame)
}

/// Read one length-prefixed control record.
// Used by the Unix authenticated transport and by the cross-platform
// state-machine tests. On non-Unix the shipped transport fails closed before
// framing, so the non-test lib has no caller.
#[cfg_attr(not(unix), allow(dead_code))]
pub(super) fn receive_record<R: Read + ?Sized>(
    reader: &mut R,
) -> Result<ControlRecord, ControlIoError> {
    let mut prefix = [0_u8; CONTROL_FRAME_PREFIX_BYTES];
    read_prefix(reader, &mut prefix)?;
    let length =
        usize::try_from(u32::from_be_bytes(prefix)).map_err(|_| ControlIoError::FrameTooLarge)?;
    if length == 0 {
        return Err(ControlIoError::ZeroLength);
    }
    if length > CONTROL_FRAME_MAX_BYTES {
        return Err(ControlIoError::FrameTooLarge);
    }
    if length < CONTROL_HEADER_BYTES {
        return Err(ControlIoError::FrameShorterThanHeader { actual: length });
    }

    let mut frame = Vec::new();
    frame
        .try_reserve_exact(length)
        .map_err(|_| ControlIoError::AllocationFailed { requested: length })?;
    frame.resize(length, 0);
    read_body(reader, &mut frame)?;
    ControlRecord::decode(&frame).map_err(ControlIoError::Codec)
}

// Used by the Unix authenticated transport and by the cross-platform
// state-machine tests. On non-Unix the shipped transport fails closed before
// framing, so the non-test lib has no caller.
#[cfg_attr(not(unix), allow(dead_code))]
fn read_prefix<R: Read + ?Sized>(
    reader: &mut R,
    prefix: &mut [u8; CONTROL_FRAME_PREFIX_BYTES],
) -> Result<(), ControlIoError> {
    let mut filled = 0_usize;
    while filled < prefix.len() {
        match reader.read(&mut prefix[filled..]) {
            Ok(0) if filled == 0 => return Err(ControlIoError::Closed),
            Ok(0) => return Err(ControlIoError::TruncatedPrefix { received: filled }),
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(ControlIoError::Io(error)),
        }
    }
    Ok(())
}

// Used by the Unix authenticated transport and by the cross-platform
// state-machine tests. On non-Unix the shipped transport fails closed before
// framing, so the non-test lib has no caller.
#[cfg_attr(not(unix), allow(dead_code))]
fn read_body<R: Read + ?Sized>(reader: &mut R, body: &mut [u8]) -> Result<(), ControlIoError> {
    let mut filled = 0_usize;
    while filled < body.len() {
        match reader.read(&mut body[filled..]) {
            Ok(0) => {
                return Err(ControlIoError::TruncatedBody {
                    expected: body.len(),
                    received: filled,
                });
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(ControlIoError::Io(error)),
        }
    }
    Ok(())
}

// Used by the Unix authenticated transport and by the cross-platform
// state-machine tests. On non-Unix the shipped transport fails closed before
// framing, so the non-test lib has no caller.
#[cfg_attr(not(unix), allow(dead_code))]
fn write_all<W: Write + ?Sized>(writer: &mut W, mut bytes: &[u8]) -> Result<(), ControlIoError> {
    while !bytes.is_empty() {
        match writer.write(bytes) {
            Ok(0) => return Err(ControlIoError::WriteZero),
            Ok(written) => bytes = &bytes[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(ControlIoError::Io(error)),
        }
    }
    Ok(())
}
