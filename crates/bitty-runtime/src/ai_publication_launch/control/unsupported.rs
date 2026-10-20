//! Fail-closed non-Unix adapter for the publication-control transport.
//!
//! The launch seam never establishes a control session off Unix, so this
//! module only exists to keep the driver's construction API platform-neutral.
//! It can never be constructed: [`AuthenticatedControl::new`] always reports
//! [`PublicationControlError::UnsupportedPlatform`] and drops the session.

use bitty_ipc_api::provider_publication::ControlRecord;

use super::ControlRecordIo;
use super::framing::ControlIoError;
use crate::ai_publication_launch::EstablishedControl;

/// Uninhabited non-Unix control transport.
pub struct AuthenticatedControl {
    _private: (),
}

impl AuthenticatedControl {
    /// Always refuse transport construction on this platform.
    ///
    /// # Errors
    ///
    /// Always returns
    /// [`PublicationControlError::UnsupportedPlatform`](super::PublicationControlError::UnsupportedPlatform).
    pub fn new(_control: EstablishedControl) -> Result<Self, super::PublicationControlError> {
        Err(super::PublicationControlError::UnsupportedPlatform)
    }
}

impl ControlRecordIo for AuthenticatedControl {
    fn send(&mut self, _record: &ControlRecord) -> Result<(), ControlIoError> {
        Err(ControlIoError::UnsupportedPlatform)
    }

    fn receive(&mut self) -> Result<ControlRecord, ControlIoError> {
        Err(ControlIoError::UnsupportedPlatform)
    }
}
