//! Fail-closed launch-handle adapter for non-Unix platforms.
//!
//! Slice 1 has no inherited-handle implementation outside Unix. The seam
//! still compiles so callers do not need platform `cfg`s, but every
//! preparation fails with [`LaunchError::UnsupportedPlatform`] before any key
//! material or channel is created.

use std::fs::File;
use std::process::Command;
use std::time::Instant;

use bitty_network_publication::{ControlClient, ControlSession};

use super::{
    CHILD_BOOTSTRAP_DESCRIPTOR, CHILD_CONTROL_DESCRIPTOR, ChildDescriptor, ChildDescriptors,
    EstablishFailure, LaunchError,
};

/// Placeholder transport type; never constructed because preparation fails.
pub(super) type ControlStream = ();

/// Placeholder handle set; never constructed because preparation fails.
pub(super) struct Handles;

impl Handles {
    pub(super) fn create() -> Result<Self, LaunchError> {
        Err(LaunchError::UnsupportedPlatform)
    }

    pub(super) fn has_child_ends(&self) -> bool {
        false
    }

    pub(super) fn child_descriptors(&self) -> ChildDescriptors {
        ChildDescriptors::new(
            ChildDescriptor::from_number(CHILD_BOOTSTRAP_DESCRIPTOR),
            ChildDescriptor::from_number(CHILD_CONTROL_DESCRIPTOR),
        )
    }

    pub(super) fn take_bootstrap_writer(&mut self) -> Option<File> {
        None
    }

    pub(super) fn take_control_parent(&mut self) -> Option<ControlStream> {
        None
    }

    pub(super) fn close_child_ends(&mut self) {}

    pub(super) fn install_child_descriptors(
        &self,
        _command: &mut Command,
    ) -> Result<(), LaunchError> {
        Err(LaunchError::UnsupportedPlatform)
    }
}

/// Fail closed before any key material is generated.
pub(super) fn ensure_supported() -> Result<(), LaunchError> {
    Err(LaunchError::UnsupportedPlatform)
}

pub(super) fn establish(
    _stream: ControlStream,
    _client: ControlClient,
    _deadline: Instant,
) -> Result<(ControlSession, ControlStream), EstablishFailure> {
    Err(EstablishFailure::Unsupported)
}
