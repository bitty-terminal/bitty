//! Fail-closed coverage for the non-Unix launch adapter.
//!
//! Preparation must reject the platform before generating key material or
//! creating any handle; there is no partial launch to clean up.

use super::{LaunchError, LaunchIdentity, prepare};

#[test]
fn preparation_fails_closed_on_non_unix_platforms() {
    let identity = LaunchIdentity::new("bitty-ai", "1.2.3", "bitty-ai", [0xab; 32], 7);
    let error =
        prepare(identity, [0x11; 32]).expect_err("non-Unix launch handles must be unsupported");
    assert!(matches!(error, LaunchError::UnsupportedPlatform));
}
