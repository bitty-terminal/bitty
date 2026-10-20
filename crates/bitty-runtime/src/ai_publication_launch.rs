//! Core-side launch seam for authenticated provider publication (slice 1).
//!
//! This module owns the Core half of the launch sequence the Bitty AI draft
//! *Authenticated provider publication* specifies: generate fresh launch
//! material, encode the bounded launch bootstrap record, create the one-shot
//! bootstrap channel and the dedicated duplex control channel, deliver the
//! record exactly once, and establish the TLS 1.3 control session inside the
//! named establishment deadline.
//!
//! Slice 1 stops at TLS establishment. It does **not** spawn or supervise the
//! helper, ingest helper-side handles, or carry control records; the existing
//! stdio-only [`crate::ai_helper`] path is untouched. Wiring the launch into
//! the helper spawn is a follow-up task once the helper entry point accepts
//! the inherited bootstrap/control handles.
//!
//! # Handle model
//!
//! [`prepare`] returns a [`PreparedLaunch`] that owns four handles:
//!
//! | Handle | Owner before spawn | Child descriptor |
//! | --- | --- | --- |
//! | Bootstrap pipe, read end | [`PreparedLaunch`] | [`CHILD_BOOTSTRAP_DESCRIPTOR`] |
//! | Bootstrap pipe, write end | [`PreparedLaunch`] | not inherited |
//! | Control socketpair, child end | [`PreparedLaunch`] | [`CHILD_CONTROL_DESCRIPTOR`] |
//! | Control socketpair, parent end | [`PreparedLaunch`] | not inherited |
//!
//! The parent keeps both child-side ends open until the child has inherited
//! them. The caller registers [`PreparedLaunch::install_child_descriptors`],
//! spawns, then calls [`PreparedLaunch::after_spawn`] to close the parent's
//! child-side copies and the spawn-time descriptor reservation. Dropping the
//! prepared launch closes every remaining handle, so an abandoned launch
//! cannot leave a pipe half-open.
//!
//! Registration also reserves every descriptor from `0` through the highest
//! placement target in the parent with close-on-exec placeholder pipes. std's
//! close-on-exec exec-error channel is allocated inside `Command::spawn` at the
//! lowest free descriptors, so occupying the whole range places it above every
//! placement target while those descriptors stay open. Without that
//! reservation, `pre_exec` could `dup2` over the error channel and a failed
//! `exec` could be reported as a successful spawn. The placeholders never
//! cross `exec` and are closed by
//! [`PreparedLaunch::after_spawn`] after the spawn attempt, by
//! [`PreparedLaunch::abort`], or by `Drop`.
//!
//! A descriptor already owned elsewhere at reservation time cannot be claimed
//! without closing its owner's descriptor. It blocks std's error channel while
//! it stays open; a concurrent close between reservation and spawn could reopen
//! the window, so callers must not close low descriptors during the spawn
//! attempt (single-threaded launch setup does not).
//!
//! On Unix the channels are an anonymous pipe and a Unix-domain socketpair.
//! On every other platform [`prepare`] fails closed with
//! [`LaunchError::UnsupportedPlatform`]; the module still compiles so callers
//! can handle the failure without a platform `cfg` at every call site.
//!
//! # Bootstrap record
//!
//! The record is exactly one encoder output from
//! `bitty-network-publication`'s bounded [`LaunchManifest`]: bootstrap and
//! control protocol versions, both role bytes, the Core-instance and launch
//! nonces, the verified component identity, the executable digest, the
//! component protocol version, the control/data channel nonces, the fixed
//! delegation purpose [`DELEGATION_PURPOSE`], the control profile
//! [`CONTROL_PROFILE`], and the launch key-material block (both leaf
//! certificates and the helper PKCS#8 key). It contains no provider metadata.
//!
//! [`PreparedLaunch::write_bootstrap`] writes the record once, closes the
//! write end, and zeroizes the serialized buffer. A second write is rejected.
//! [`PreparedLaunch::abort`] (and `Drop`) zeroizes an unwritten buffer.
//!
//! # Deadline ownership
//!
//! The caller owns the absolute deadline. [`CONTROL_ESTABLISH_TIMEOUT`] is the
//! named five-second bound measured from spawn; the caller computes
//! `spawn_time + CONTROL_ESTABLISH_TIMEOUT` and passes it to
//! [`PreparedLaunch::establish`]. The handshake then never waits past that
//! absolute instant: every read and write is bounded by the remaining time,
//! and an expired deadline fails with [`LaunchError::EstablishTimeout`].
//!
//! # Security notes
//!
//! - Launch material and the encoded record stay in process memory. The
//!   helper private key appears only in the record and in the material's
//!   zeroizing buffers; formatting is redacted.
//! - The TLS client is pinned to the helper leaf digest, requires ALPN
//!   [`CONTROL_ALPN`], offers only TLS 1.3, and never falls back to a weaker
//!   identity.
//! - Descriptor numbers are fixed launch conventions, not secrets. No host
//!   path, network listener, or environment value enters this module.
//!
//! Types from `bitty-network-publication` stay private to this module:
//! following the crate's ADR-0004 ownership rule, no upstream type appears in
//! a public signature.

use std::fmt;
use std::io::{self, Write};
use std::process::Command;
use std::time::Instant;

use bitty_network_publication::{
    CONTROL_STRING_MAX_BYTES, LaunchIdentity as PublicationIdentity, LaunchManifest, LaunchMaterial,
};
use zeroize::{Zeroize, Zeroizing};

#[cfg(unix)]
#[path = "ai_publication_launch/unix.rs"]
mod platform;
#[cfg(not(unix))]
#[path = "ai_publication_launch/unsupported.rs"]
mod platform;

/// Fixed child descriptor number of the one-shot bootstrap read end.
pub const CHILD_BOOTSTRAP_DESCRIPTOR: u32 = 3;

/// Fixed child descriptor number of the dedicated duplex control stream.
///
/// Descriptor `5` stays reserved for the data channel in a later slice.
pub const CHILD_CONTROL_DESCRIPTOR: u32 = 4;

/// Sole delegation purpose carried by a provider-publication launch.
pub const DELEGATION_PURPOSE: &str = bitty_network_publication::DELEGATION_PURPOSE;

/// Control profile string carried by a provider-publication launch.
pub const CONTROL_PROFILE: &str = bitty_network_publication::CONTROL_PROFILE;

/// ALPN protocol selected on the dedicated control stream.
pub const CONTROL_ALPN: &[u8] = bitty_network_publication::CONTROL_ALPN;

/// Length of a Core-instance, launch, or channel nonce, in bytes.
pub const LAUNCH_NONCE_BYTES: usize = 32;

/// Named establishment bound: five seconds from helper spawn.
pub const CONTROL_ESTABLISH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

// The upstream manifest encoder requires exactly this nonce width. Keeping
// the assertion here makes a future upstream change fail at compile time
// rather than at a runtime conversion.
const _: () = assert!(LAUNCH_NONCE_BYTES == bitty_network_publication::RANDOM_ID_BYTES);

/// Verified component identity bound into the launch bootstrap record.
///
/// This mirrors the manifest identity but is owned by the runtime so no
/// upstream type crosses the module boundary. Strings are non-empty UTF-8 and
/// at most 128 bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchIdentity {
    component_name: String,
    descriptor_version: String,
    executable_name: String,
    executable_sha256: [u8; 32],
    component_protocol_version: u16,
}

impl LaunchIdentity {
    /// Build a launch identity from verified supervisor metadata.
    ///
    /// The caller supplies values already checked against the component
    /// descriptor; [`prepare`] re-validates the string bounds before any key
    /// material is generated.
    pub fn new(
        component_name: impl Into<String>,
        descriptor_version: impl Into<String>,
        executable_name: impl Into<String>,
        executable_sha256: [u8; 32],
        component_protocol_version: u16,
    ) -> Self {
        Self {
            component_name: component_name.into(),
            descriptor_version: descriptor_version.into(),
            executable_name: executable_name.into(),
            executable_sha256,
            component_protocol_version,
        }
    }

    /// Component name from the verified descriptor.
    pub fn component_name(&self) -> &str {
        &self.component_name
    }

    /// Exact descriptor version.
    pub fn descriptor_version(&self) -> &str {
        &self.descriptor_version
    }

    /// Executable name from the verified descriptor.
    pub fn executable_name(&self) -> &str {
        &self.executable_name
    }

    /// SHA-256 of the verified component executable.
    pub fn executable_sha256(&self) -> &[u8; 32] {
        &self.executable_sha256
    }

    /// Selected component protocol version.
    pub fn component_protocol_version(&self) -> u16 {
        self.component_protocol_version
    }

    fn to_publication_identity(&self) -> PublicationIdentity {
        PublicationIdentity::new(
            self.component_name.clone(),
            self.descriptor_version.clone(),
            self.executable_name.clone(),
            self.executable_sha256,
            self.component_protocol_version,
        )
    }
}

/// Child-side descriptor number for one inherited launch channel.
///
/// Unix values are ordinary file-descriptor numbers; the wrapper keeps the
/// public API platform-neutral so non-Unix callers can compile against the
/// same seam while [`prepare`] fails closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChildDescriptor(u64);

impl ChildDescriptor {
    /// The raw descriptor number.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The descriptor as a Unix raw descriptor.
    #[cfg(unix)]
    pub fn as_raw_fd(self) -> std::os::fd::RawFd {
        // Descriptor numbers are non-negative on every supported Unix.
        self.0 as std::os::fd::RawFd
    }

    pub(crate) const fn from_number(number: u32) -> Self {
        Self(number as u64)
    }
}

/// Both child-side descriptor numbers of one prepared launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChildDescriptors {
    bootstrap: ChildDescriptor,
    control: ChildDescriptor,
}

impl ChildDescriptors {
    /// One-shot bootstrap read end.
    pub const fn bootstrap(self) -> ChildDescriptor {
        self.bootstrap
    }

    /// Dedicated duplex control stream.
    pub const fn control(self) -> ChildDescriptor {
        self.control
    }

    pub(crate) const fn new(bootstrap: ChildDescriptor, control: ChildDescriptor) -> Self {
        Self { bootstrap, control }
    }
}

/// Why a prepared launch cannot proceed.
///
/// Variants carry no key material, certificate bytes, or nonce values. A
/// failure after the launch has started is launch-fatal: the caller discards
/// the helper process and retries only with fresh material.
#[derive(Debug)]
pub enum LaunchError {
    /// This build cannot create inherited handles on this platform.
    UnsupportedPlatform,
    /// The operating-system random source failed.
    Random,
    /// Fresh launch key material could not be generated.
    Material(String),
    /// The identity or encoded record violated a named manifest bound.
    Manifest(String),
    /// The bootstrap pipe or control socketpair could not be created.
    Channel(io::Error),
    /// The requested handle was already consumed, closed, or never created.
    HandleMissing,
    /// The parent-side spawn descriptor reservation could not be created.
    SpawnReservation(io::Error),
    /// [`PreparedLaunch::establish`] ran before the bootstrap record was written.
    BootstrapNotWritten,
    /// [`PreparedLaunch::write_bootstrap`] ran more than once.
    BootstrapAlreadyWritten,
    /// Writing or flushing the one-shot bootstrap record failed.
    BootstrapWrite(io::Error),
    /// The TLS client configuration could not be built.
    ControlConfiguration(String),
    /// The helper refused or failed the TLS handshake.
    TlsRefused(TlsRefusal),
    /// The absolute establishment deadline expired.
    EstablishTimeout,
    /// The launch was aborted before the requested operation.
    Aborted,
}

impl fmt::Display for LaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform => {
                formatter.write_str("publication launch handles are unsupported on this platform")
            }
            Self::Random => formatter.write_str("launch nonce generation failed"),
            Self::Material(detail) => write!(formatter, "launch material failed: {detail}"),
            Self::Manifest(detail) => write!(formatter, "launch manifest rejected: {detail}"),
            Self::Channel(error) => write!(formatter, "launch channel setup failed: {error}"),
            Self::HandleMissing => {
                formatter.write_str("launch handle is missing or already closed")
            }
            Self::SpawnReservation(error) => {
                write!(formatter, "spawn descriptor reservation failed: {error}")
            }
            Self::BootstrapNotWritten => {
                formatter.write_str("launch bootstrap record has not been written")
            }
            Self::BootstrapAlreadyWritten => {
                formatter.write_str("launch bootstrap record was already written")
            }
            Self::BootstrapWrite(error) => {
                write!(formatter, "launch bootstrap write failed: {error}")
            }
            Self::ControlConfiguration(detail) => {
                write!(formatter, "control TLS configuration failed: {detail}")
            }
            Self::TlsRefused(reason) => write!(formatter, "control TLS refused: {reason}"),
            Self::EstablishTimeout => formatter.write_str("control establishment deadline expired"),
            Self::Aborted => formatter.write_str("launch was aborted"),
        }
    }
}

impl std::error::Error for LaunchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Channel(error) | Self::SpawnReservation(error) | Self::BootstrapWrite(error) => {
                Some(error)
            }
            Self::TlsRefused(reason) => reason.source(),
            _ => None,
        }
    }
}

/// Why the helper-side TLS endpoint was not accepted.
///
/// A verifier pin rejection surfaces as [`Self::Io`] with the rustls alert,
/// exactly like any other fatal handshake failure. Callers must treat every
/// variant as launch-fatal; there is no retry with a weaker identity.
#[derive(Debug)]
pub enum TlsRefusal {
    /// The control stream failed or TLS aborted the handshake.
    Io(io::Error),
    /// The handshake made no progress while still incomplete.
    HandshakeStalled,
    /// The peer did not negotiate [`CONTROL_ALPN`].
    AlpnMismatch,
    /// The connection completed without a peer certificate.
    NoPeerCertificate,
    /// The peer presented more than the one accepted end-entity certificate.
    UnexpectedPeerCertificateCount(usize),
}

impl fmt::Display for TlsRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "control TLS stream failed: {error}"),
            Self::HandshakeStalled => formatter.write_str("control TLS handshake stalled"),
            Self::AlpnMismatch => formatter.write_str("control TLS ALPN mismatch"),
            Self::NoPeerCertificate => formatter.write_str("control TLS peer sent no certificate"),
            Self::UnexpectedPeerCertificateCount(count) => write!(
                formatter,
                "control TLS peer sent {count} certificates, expected exactly one"
            ),
        }
    }
}

impl std::error::Error for TlsRefusal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchState {
    Prepared,
    Bootstrapped,
    Established,
    Aborted,
}

/// A launch whose material, record, and channels are ready for delivery.
///
/// The value is single-use: [`Self::write_bootstrap`] succeeds exactly once,
/// and [`Self::establish`] consumes the material and the parent control
/// stream. Dropping an unused or partially used value zeroizes its record and
/// closes every handle.
pub struct PreparedLaunch {
    identity: LaunchIdentity,
    core_instance_nonce: [u8; LAUNCH_NONCE_BYTES],
    launch_nonce: [u8; LAUNCH_NONCE_BYTES],
    control_channel_nonce: [u8; LAUNCH_NONCE_BYTES],
    data_channel_nonce: [u8; LAUNCH_NONCE_BYTES],
    material: Option<LaunchMaterial>,
    record: Option<Zeroizing<Vec<u8>>>,
    handles: Option<platform::Handles>,
    state: LaunchState,
}

impl PreparedLaunch {
    /// Verified component identity bound into this launch.
    pub fn identity(&self) -> &LaunchIdentity {
        &self.identity
    }

    /// Core-instance nonce supplied at preparation time.
    pub fn core_instance_nonce(&self) -> &[u8; LAUNCH_NONCE_BYTES] {
        &self.core_instance_nonce
    }

    /// Fresh launch nonce.
    pub fn launch_nonce(&self) -> &[u8; LAUNCH_NONCE_BYTES] {
        &self.launch_nonce
    }

    /// Fresh control-channel nonce.
    pub fn control_channel_nonce(&self) -> &[u8; LAUNCH_NONCE_BYTES] {
        &self.control_channel_nonce
    }

    /// Fresh data-channel nonce.
    pub fn data_channel_nonce(&self) -> &[u8; LAUNCH_NONCE_BYTES] {
        &self.data_channel_nonce
    }

    /// The helper leaf pin this launch will present to its TLS verifier.
    ///
    /// `None` once [`Self::establish`] has consumed the launch material.
    pub fn helper_cert_sha256(&self) -> Option<[u8; 32]> {
        self.material
            .as_ref()
            .map(LaunchMaterial::helper_cert_sha256)
    }

    /// Fixed child-side descriptor numbers for this launch.
    ///
    /// Fails with [`LaunchError::HandleMissing`] after [`Self::after_spawn`]
    /// or [`Self::abort`] has closed the child-side ends.
    pub fn child_descriptors(&self) -> Result<ChildDescriptors, LaunchError> {
        match self.handles.as_ref() {
            Some(handles) if handles.has_child_ends() => Ok(handles.child_descriptors()),
            _ => Err(LaunchError::HandleMissing),
        }
    }

    /// Register the pre-exec placement of both child-side descriptors.
    ///
    /// The hook duplicates the launch's child ends to
    /// [`CHILD_BOOTSTRAP_DESCRIPTOR`] and [`CHILD_CONTROL_DESCRIPTOR`],
    /// clearing close-on-exec on the placed copies. It runs post-fork in the
    /// single-threaded child and fails the spawn closed on any error.
    ///
    /// On Unix, registration first reserves the whole `0..=highest` placement
    /// range in the parent with close-on-exec placeholder pipes. The
    /// reservation must be held across `Command::spawn` so std's exec-error
    /// channel is allocated above the placement targets; [`Self::after_spawn`]
    /// releases it after the spawn attempt, and dropping the launch releases
    /// it otherwise.
    pub fn install_child_descriptors(&mut self, command: &mut Command) -> Result<(), LaunchError> {
        self.handles
            .as_mut()
            .ok_or(LaunchError::HandleMissing)?
            .install_child_descriptors(command)
    }

    /// Close the parent's child-side descriptor copies after a spawn attempt.
    ///
    /// Call this after `Command::spawn` returns, whether it succeeded or
    /// failed: a failed spawn leaves the launch unusable, and the spawn-time
    /// descriptor reservation must not outlive the attempt. Idempotent.
    /// Dropping the prepared launch performs the same cleanup, so an early
    /// return cannot leak the reservation.
    pub fn after_spawn(&mut self) {
        if let Some(handles) = self.handles.as_mut() {
            handles.close_child_ends();
        }
    }

    /// Write the bootstrap record exactly once and close the write end.
    ///
    /// The serialized buffer is zeroized before this method returns, whether
    /// the write succeeded or failed. A second call fails with
    /// [`LaunchError::BootstrapAlreadyWritten`]; any write failure is
    /// launch-fatal and closes the remaining handles.
    pub fn write_bootstrap(&mut self) -> Result<(), LaunchError> {
        match self.state {
            LaunchState::Prepared => {}
            LaunchState::Bootstrapped | LaunchState::Established => {
                return Err(LaunchError::BootstrapAlreadyWritten);
            }
            LaunchState::Aborted => return Err(LaunchError::Aborted),
        }

        let mut record = self.record.take().ok_or(LaunchError::HandleMissing)?;
        let mut writer = self
            .handles
            .as_mut()
            .and_then(platform::Handles::take_bootstrap_writer)
            .ok_or(LaunchError::HandleMissing)?;
        let result = write_and_zeroize(&mut writer, &mut record);
        // Close the write end before reporting, even on failure: the helper
        // must never observe a half-written record followed by an open pipe.
        drop(writer);
        drop(record);

        match result {
            Ok(()) => {
                self.state = LaunchState::Bootstrapped;
                Ok(())
            }
            Err(error) => {
                self.handles = None;
                self.material = None;
                self.state = LaunchState::Aborted;
                Err(LaunchError::BootstrapWrite(error))
            }
        }
    }

    /// Establish the pinned TLS 1.3 control session before `deadline`.
    ///
    /// The bootstrap record must already be written. `deadline` is an absolute
    /// instant; callers derive it as `spawn_time + CONTROL_ESTABLISH_TIMEOUT`.
    /// On success the returned session owns the parent control stream and the
    /// negotiated ALPN is [`CONTROL_ALPN`]. Every failure is launch-fatal and
    /// closes the remaining handles.
    pub fn establish(&mut self, deadline: Instant) -> Result<EstablishedControl, LaunchError> {
        self.establish_inner(deadline, None)
    }

    /// Test-only establishment with an explicitly supplied helper pin, so the
    /// pin-refusal outcome is exercised through the production seam.
    ///
    /// Gated on the fixture feature, not just `test`: `--all-targets` builds
    /// the lib test harness without `publication-control-test-support`, where
    /// the only caller (the fixture module) is compiled out.
    #[cfg(all(test, unix, feature = "publication-control-test-support"))]
    fn establish_with_expected_pin(
        &mut self,
        deadline: Instant,
        expected_helper_pin: [u8; 32],
    ) -> Result<EstablishedControl, LaunchError> {
        self.establish_inner(deadline, Some(expected_helper_pin))
    }

    fn establish_inner(
        &mut self,
        deadline: Instant,
        expected_helper_pin: Option<[u8; 32]>,
    ) -> Result<EstablishedControl, LaunchError> {
        match self.state {
            LaunchState::Bootstrapped => {}
            LaunchState::Prepared => return Err(LaunchError::BootstrapNotWritten),
            LaunchState::Established => return Err(LaunchError::HandleMissing),
            LaunchState::Aborted => return Err(LaunchError::Aborted),
        }

        if Instant::now() >= deadline {
            self.abort();
            return Err(LaunchError::EstablishTimeout);
        }

        let material = self.material.take().ok_or(LaunchError::HandleMissing)?;
        let helper_pin = material.helper_cert_sha256();
        let client = match expected_helper_pin {
            Some(pin) => {
                bitty_network_publication::ControlClient::new_with_expected_pin(material, pin)
            }
            None => bitty_network_publication::ControlClient::new(material),
        }
        .map_err(|error| {
            let detail = error.to_string();
            self.abort();
            LaunchError::ControlConfiguration(detail)
        })?;
        let Some(stream) = self
            .handles
            .as_mut()
            .and_then(platform::Handles::take_control_parent)
        else {
            self.abort();
            return Err(LaunchError::HandleMissing);
        };

        match platform::establish(stream, client, deadline) {
            Ok((session, stream)) => {
                self.state = LaunchState::Established;
                Ok(EstablishedControl::new(session, stream, helper_pin))
            }
            Err(failure) => {
                self.abort();
                Err(failure.into_launch_error())
            }
        }
    }

    /// Abort the launch: zeroize the record and close every handle.
    ///
    /// Idempotent. Called automatically by `Drop`.
    pub fn abort(&mut self) {
        self.record = None;
        self.material = None;
        self.handles = None;
        self.state = LaunchState::Aborted;
    }
}

impl fmt::Debug for PreparedLaunch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedLaunch")
            .field("identity", &self.identity)
            .field("state", &self.state)
            .field(
                "record_len",
                &self.record.as_ref().map(|record| record.len()),
            )
            .field("helper_cert_sha256", &self.helper_cert_sha256())
            .finish()
    }
}

impl Drop for PreparedLaunch {
    fn drop(&mut self) {
        self.abort();
    }
}

/// A completed, pinned TLS 1.3 control session on the parent stream.
///
/// The underlying rustls connection and the parent control stream stay
/// private. A later slice can extend this type with the bounded control-record
/// methods; keeping the transport here means an established session cannot be
/// separated from its authenticated channel.
pub struct EstablishedControl {
    session: bitty_network_publication::ControlSession,
    /// Present only where a real transport exists: the non-Unix adapter never
    /// establishes, so its `ControlStream` placeholder is never stored.
    #[cfg(unix)]
    stream: platform::ControlStream,
    helper_cert_sha256: [u8; 32],
}

impl EstablishedControl {
    fn new(
        session: bitty_network_publication::ControlSession,
        stream: platform::ControlStream,
        helper_cert_sha256: [u8; 32],
    ) -> Self {
        // Keep the constructor signature platform-neutral: on non-Unix the
        // adapter cannot establish, so the placeholder stream is discarded
        // instead of being stored in a field that does not exist there.
        #[cfg(not(unix))]
        let _ = stream;
        Self {
            session,
            #[cfg(unix)]
            stream,
            helper_cert_sha256,
        }
    }

    /// The negotiated ALPN protocol, always [`CONTROL_ALPN`] on success.
    pub fn alpn_protocol(&self) -> Option<&[u8]> {
        self.session.alpn_protocol()
    }

    /// Whether the connection is still handshaking; always `false` here.
    pub fn is_handshaking(&self) -> bool {
        self.session.is_handshaking()
    }

    /// The helper leaf pin the client verified.
    pub fn helper_cert_sha256(&self) -> [u8; 32] {
        self.helper_cert_sha256
    }

    /// Borrow the established parent control stream.
    #[cfg(unix)]
    pub fn stream(&self) -> &std::os::unix::net::UnixStream {
        &self.stream
    }

    /// Mutably borrow the established parent control stream.
    #[cfg(unix)]
    pub fn stream_mut(&mut self) -> &mut std::os::unix::net::UnixStream {
        &mut self.stream
    }
}

impl fmt::Debug for EstablishedControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EstablishedControl")
            .field("alpn_protocol", &self.session.alpn_protocol())
            .field("helper_cert_sha256", &self.helper_cert_sha256)
            .finish()
    }
}

/// Prepare one helper launch from a verified component identity.
///
/// Generates fresh launch material, four nonces (three fresh, plus the
/// caller's Core-instance nonce), the bounded bootstrap record, and both
/// channels. On non-Unix platforms this fails closed with
/// [`LaunchError::UnsupportedPlatform`] before generating key material.
pub fn prepare(
    identity: LaunchIdentity,
    core_instance_nonce: [u8; LAUNCH_NONCE_BYTES],
) -> Result<PreparedLaunch, LaunchError> {
    platform::ensure_supported()?;
    validate_identity_string("component name", &identity.component_name)?;
    validate_identity_string("descriptor version", &identity.descriptor_version)?;
    validate_identity_string("executable name", &identity.executable_name)?;

    let launch_nonce = random_nonce()?;
    let control_channel_nonce = random_nonce()?;
    let data_channel_nonce = random_nonce()?;

    let material =
        LaunchMaterial::generate().map_err(|error| LaunchError::Material(error.to_string()))?;
    let manifest = LaunchManifest::new(
        core_instance_nonce,
        launch_nonce,
        identity.to_publication_identity(),
        control_channel_nonce,
        data_channel_nonce,
    )
    .map_err(|error| LaunchError::Manifest(error.to_string()))?;
    let record = manifest
        .encode(&material)
        .map_err(|error| LaunchError::Manifest(error.to_string()))?;
    let handles = platform::Handles::create()?;

    Ok(PreparedLaunch {
        identity,
        core_instance_nonce,
        launch_nonce,
        control_channel_nonce,
        data_channel_nonce,
        material: Some(material),
        record: Some(record),
        handles: Some(handles),
        state: LaunchState::Prepared,
    })
}

/// Write `record`, flush it, and zeroize the buffer unconditionally.
///
/// Kept separate from [`PreparedLaunch::write_bootstrap`] so the failure path
/// can prove zeroization without a live pipe: the caller owns `record` after
/// the call in both the success and failure cases.
fn write_and_zeroize<W: Write>(writer: &mut W, record: &mut Zeroizing<Vec<u8>>) -> io::Result<()> {
    let result = writer
        .write_all(record.as_slice())
        .and_then(|()| writer.flush());
    record.zeroize();
    result
}

fn random_nonce() -> Result<[u8; LAUNCH_NONCE_BYTES], LaunchError> {
    let mut nonce = [0u8; LAUNCH_NONCE_BYTES];
    getrandom::fill(&mut nonce).map_err(|_| LaunchError::Random)?;
    Ok(nonce)
}

fn validate_identity_string(field: &'static str, value: &str) -> Result<(), LaunchError> {
    if value.is_empty() {
        return Err(LaunchError::Manifest(format!(
            "manifest field {field} is empty"
        )));
    }
    if value.len() > CONTROL_STRING_MAX_BYTES {
        return Err(LaunchError::Manifest(format!(
            "manifest field {field} is {} bytes, maximum {CONTROL_STRING_MAX_BYTES}",
            value.len()
        )));
    }
    Ok(())
}

/// Internal establishment outcome, shared by both platform adapters.
///
/// Every variant is `cfg`-gated on the adapter that constructs it: a shared
/// enum would otherwise carry variants that are dead code on one platform,
/// which `-D warnings` rejects in the Windows feature-graph check.
enum EstablishFailure {
    /// Constructed only by the non-Unix adapter.
    #[cfg(not(unix))]
    Unsupported,
    /// Constructed only by the Unix adapter's deadline path.
    #[cfg(unix)]
    Timeout,
    /// Constructed only by the Unix adapter's TLS path.
    #[cfg(unix)]
    Tls(bitty_network_publication::ControlError),
}

impl EstablishFailure {
    fn into_launch_error(self) -> LaunchError {
        match self {
            #[cfg(not(unix))]
            Self::Unsupported => LaunchError::UnsupportedPlatform,
            #[cfg(unix)]
            Self::Timeout => LaunchError::EstablishTimeout,
            #[cfg(unix)]
            Self::Tls(error) => LaunchError::TlsRefused(TlsRefusal::from(error)),
        }
    }
}

impl From<bitty_network_publication::ControlError> for TlsRefusal {
    fn from(error: bitty_network_publication::ControlError) -> Self {
        use bitty_network_publication::ControlError;
        match error {
            ControlError::Io(error) => Self::Io(error),
            ControlError::HandshakeStalled => Self::HandshakeStalled,
            ControlError::AlpnMismatch => Self::AlpnMismatch,
            ControlError::NoPeerCertificate => Self::NoPeerCertificate,
            ControlError::UnexpectedPeerCertificateCount { count } => {
                Self::UnexpectedPeerCertificateCount(count)
            }
            // Configuration failures are handled before the handshake; if one
            // ever reaches this point, flatten it without key material.
            ControlError::Configuration(error) | ControlError::ClientIdentity(error) => {
                Self::Io(io::Error::other(error.to_string()))
            }
            ControlError::ServerName => {
                Self::Io(io::Error::other("control TLS server name rejected"))
            }
        }
    }
}

#[cfg(all(test, feature = "publication-control-test-support", unix))]
mod tests;
#[cfg(all(test, feature = "publication-control-test-support", not(unix)))]
mod unsupported_tests;
