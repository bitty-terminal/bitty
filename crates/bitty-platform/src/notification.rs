//! The audible bell primitive (CTX-0754, issue #1361).
//!
//! `BellMode::Audible` had no OS primitive at all before this module. This is
//! the fail-closed delivery floor underneath the runtime policy
//! (`bitty-runtime` gates consent, `RC-8` rate, and queue bounds before
//! anything here runs):
//!
//! - [`BellSink`]: injectable seam (the same pattern as
//!   [`crate::clipboard::Clipboard`] doubles and the runtime's `UrlOpener`).
//!   Tests install a recording double; production uses [`OsBellSink`].
//! - [`OsBellSink`]: best-effort audible bell. Without an audio-synthesis
//!   dependency the portable primitive is a single `BEL` byte on stderr, so
//!   a hosting terminal (or console) can sound it; embedders that own audio
//!   replace the sink. Richer audio awaits the `OQ-076` policy decision.
//!
//! Desktop-notification delivery used to live here too
//! (`OsNotificationSink`, spawning `notify-send` / `osascript` with a
//! bounded reaper). CTX-1178 (issue #1998) retired that second
//! implementation onto the real `bitty-platform-services` backends, which
//! own the fixed-argv spawn, the bounded reaper, and the fail-closed
//! backend mapping behind their delivery contract. Core keeps the in-app
//! banner plus rate limiting; OS delivery for notifications flows through
//! the services bridge only, so a single rate/dispatch path remains.
//!
//! Every outcome is an [`OsDeliveryOutcome`]: delivery is best-effort and
//! fail-closed (I/O error is a value, never a panic).
//!
//! Headless note: unit tests exercise only pure constructors. No test sounds
//! a real bell.

use std::io::Write as _;

/// Best-effort OS delivery outcome.
///
/// `Delivered` means the request reached the OS backend; the OS may still
/// drop it silently, which no synchronous API can observe without blocking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OsDeliveryOutcome {
    /// The backend accepted the request.
    Delivered,
    /// No attempt was made (backend absent, no session, or disabled).
    Skipped(OsDeliverySkip),
    /// The attempt failed; carries the backend diagnostic.
    Failed(String),
}

/// Why a delivery attempt was skipped without touching the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsDeliverySkip {
    /// No usable OS backend exists on this platform/configuration.
    BackendMissing,
    /// Delivery was disabled by the caller (reserved for embedder gating).
    Disabled,
}

/// Injectable audible-bell seam.
///
/// Production uses [`OsBellSink`]; tests install a recording double.
pub trait BellSink {
    /// Sounds one admitted audible bell (best-effort).
    fn ring(&self) -> OsDeliveryOutcome;
}

/// Production [`BellSink`]: best-effort audible bell.
///
/// Writes a single `BEL` byte (`0x07`) to stderr so a hosting terminal or
/// console can sound it. This is the portable floor without an
/// audio-synthesis dependency — not a synthesized tone; richer audio awaits
/// the `OQ-076` policy decision. I/O failure is a [`OsDeliveryOutcome::Failed`]
/// value, never a panic.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsBellSink;

impl BellSink for OsBellSink {
    fn ring(&self) -> OsDeliveryOutcome {
        let mut stderr = std::io::stderr().lock();
        match stderr.write_all(&[0x07]) {
            Ok(()) => OsDeliveryOutcome::Delivered,
            Err(error) => OsDeliveryOutcome::Failed(error.to_string()),
        }
    }
}
