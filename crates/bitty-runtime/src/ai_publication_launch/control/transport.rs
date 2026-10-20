//! Authenticated record transport for the Core publication-control driver.
//!
//! The adapter borrows nothing: it owns the [`EstablishedControl`] session, so
//! dropping the driver closes the TLS 1.3 stream. Reads and writes use the
//! rustls connection behind [`EstablishedControl`]; this module adds only the
//! bounded outer framing and never opens a socket, listener, thread, or clock.
//!
//! # Deadlines
//!
//! I/O is blocking, exactly like the launch handshake. The caller owns the
//! absolute transaction and session deadlines: configure the underlying
//! stream's read/write timeouts (or its nonblocking policy) before driving the
//! state machine. A timed-out or would-block read surfaces as
//! [`ControlIoError::Io`] and revokes authority; the driver never retries.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

use bitty_ipc_api::provider_publication::ControlRecord;

use super::ControlRecordIo;
use super::framing::{ControlIoError, receive_record, send_record};
use crate::ai_publication_launch::EstablishedControl;

/// Maximum TLS records one plaintext read may consume before yielding.
///
/// A 256 KiB control frame spans at most sixteen 16 KiB TLS records. The
/// margin also covers a bounded run of KeyUpdate or alert records; a peer that
/// keeps sending non-application records past this bound is refused instead of
/// being drained in one call.
const MAX_TLS_RECORDS_PER_READ: usize = 64;

/// Record transport over one authenticated publication-control session.
pub struct AuthenticatedControl {
    control: EstablishedControl,
}

impl AuthenticatedControl {
    /// Wrap a completed, pinned TLS 1.3 control session.
    ///
    /// # Errors
    ///
    /// This adapter cannot fail on Unix; the result keeps the signature
    /// platform-neutral so a non-Unix caller observes
    /// [`super::PublicationControlError::UnsupportedPlatform`] instead of a
    /// compile-time branch.
    pub fn new(control: EstablishedControl) -> Result<Self, super::PublicationControlError> {
        Ok(Self { control })
    }
}

impl ControlRecordIo for AuthenticatedControl {
    fn send(&mut self, record: &ControlRecord) -> Result<(), ControlIoError> {
        let EstablishedControl {
            session, stream, ..
        } = &mut self.control;
        let mut plaintext = TlsPlaintext { session, stream };
        send_record(&mut plaintext, record)
    }

    fn receive(&mut self) -> Result<ControlRecord, ControlIoError> {
        let EstablishedControl {
            session, stream, ..
        } = &mut self.control;
        let mut plaintext = TlsPlaintext { session, stream };
        receive_record(&mut plaintext)
    }
}

impl std::fmt::Debug for AuthenticatedControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthenticatedControl")
            .field("alpn_protocol", &self.control.alpn_protocol())
            .finish_non_exhaustive()
    }
}

/// Plaintext view over the established TLS connection and its stream.
struct TlsPlaintext<'a> {
    session: &'a mut bitty_network_publication::ControlSession,
    stream: &'a mut UnixStream,
}

impl TlsPlaintext<'_> {
    fn write_tls(&mut self) -> io::Result<()> {
        loop {
            let connection = self.session.connection_mut();
            if !connection.wants_write() {
                return Ok(());
            }
            match connection.write_tls(self.stream) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "control TLS write made no progress",
                    ));
                }
                Ok(_) => {}
                Err(error) => return Err(error),
            }
        }
    }
}

impl Read for TlsPlaintext<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut records = 0_usize;
        loop {
            let blocked = {
                let connection = self.session.connection_mut();
                match connection.reader().read(buffer) {
                    Ok(read) => return Ok(read),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => true,
                    Err(error) => return Err(error),
                }
            };
            debug_assert!(blocked);

            if records >= MAX_TLS_RECORDS_PER_READ {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "control read consumed too many TLS records without plaintext",
                ));
            }
            records += 1;

            // Read at most one buffer of ciphertext and process only complete
            // records. Unlike `complete_io`, this never drains an unbounded
            // run of non-application records inside one call.
            let read = self.session.connection_mut().read_tls(self.stream)?;
            if read == 0 {
                return Ok(0);
            }
            self.session
                .connection_mut()
                .process_new_packets()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        }
    }
}

impl Write for TlsPlaintext<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.session.connection_mut().writer().write(buffer)?;
        self.write_tls()?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.write_tls()?;
        self.stream.flush()
    }
}
