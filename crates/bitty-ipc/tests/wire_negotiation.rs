#![forbid(unsafe_code)]
//! Wire version negotiation (CTX-0484, IPC and Agent RFC §Versioning).
//!
//! The RFC requires the runtime to advertise its wire version and select the
//! highest version both peers support; the envelope remains fail-closed for
//! versions outside the supported set (`v1` today).

use bitty_ipc::{
    SUPPORTED_WIRE_VERSIONS, WIRE_VERSION, negotiate_wire_version, validate_wire_version,
};

#[test]
fn advertised_versions_are_ordered_and_include_the_current_wire_version() {
    assert!(
        SUPPORTED_WIRE_VERSIONS.contains(&WIRE_VERSION),
        "the advertised set must include the current contract version"
    );
    for pair in SUPPORTED_WIRE_VERSIONS.windows(2) {
        assert!(
            pair[0] > pair[1],
            "advertised versions must be highest-first for negotiation: {SUPPORTED_WIRE_VERSIONS:?}"
        );
    }
}

#[test]
fn negotiation_selects_the_highest_mutual_version() {
    assert_eq!(
        negotiate_wire_version(&[WIRE_VERSION]).expect("client speaks v1"),
        WIRE_VERSION
    );
    // A newer client still negotiates down to the highest version both support.
    assert_eq!(
        negotiate_wire_version(&[WIRE_VERSION, 2, 3]).expect("overlap on v1"),
        WIRE_VERSION
    );
    // Ordering is irrelevant to the result.
    assert_eq!(
        negotiate_wire_version(&[3, 1]).expect("overlap on v1"),
        WIRE_VERSION
    );
}

#[test]
fn negotiation_fails_closed_without_overlap() {
    for offered in [&[][..], &[0], &[2], &[2, 3]] {
        let err = negotiate_wire_version(offered).expect_err("no overlap must fail closed");
        assert!(
            matches!(err, bitty_ipc::IpcError::VersionMismatch { .. }),
            "expected VersionMismatch for {offered:?}, got {err:?}"
        );
    }
}

#[test]
fn envelope_validation_stays_fail_closed_for_unsupported_versions() {
    assert!(validate_wire_version(WIRE_VERSION).is_ok());
    assert!(validate_wire_version(0).is_err());
    assert!(validate_wire_version(2).is_err());
}
