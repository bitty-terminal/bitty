//! CTX-0493 regression: plugin-host feedback on compat shorthand ranges and
//! `u32::MAX` bound arithmetic.
//!
//! Reported in-use spellings (bitty-plugins, official entries and manifests)
//! must evaluate as their zero-padded full versions, and unrepresentable
//! upper bounds must fail with a clean error instead of panicking.

#![forbid(unsafe_code)]

use bitty_package::{
    Compat, PackageId, PackageIdentity, PackageManifest, Version, VersionReq, check_compatibility,
};

fn manifest_with_bitty_compat(req: &str) -> PackageManifest {
    PackageManifest {
        identity: PackageIdentity {
            id: PackageId::new("xuepoo.ctx0493").unwrap(),
            name: "CTX-0493".to_string(),
            version: "1.0.0".to_string(),
            description: "compat shorthand regression".to_string(),
            license: Some("MIT".to_string()),
        },
        compat: Compat {
            bitty: Some(req.to_string()),
            plugin_api: None,
        },
        dependencies: Vec::new(),
        capabilities: Vec::new(),
        raw_bytes_len: 256,
        undeclared_fields: Vec::new(),
    }
}

#[test]
fn reported_partial_comparator_range_evaluates() {
    let req = VersionReq::parse(">=0.5,<1.0").expect(">=0.5,<1.0 must parse");
    let mut bounds: Vec<String> = req
        .comparators
        .iter()
        .map(|comparator| comparator.version.to_string())
        .collect();
    bounds.sort();
    assert_eq!(bounds, vec!["0.5.0", "1.0.0"]);
    assert!(req.matches(&Version::parse("0.6.0").unwrap()));
    assert!(req.matches(&Version::parse("0.9.9").unwrap()));
    assert!(!req.matches(&Version::parse("0.4.9").unwrap()));
    assert!(!req.matches(&Version::parse("1.0.0").unwrap()));
}

#[test]
fn reported_partial_single_comparator_evaluates() {
    let req = VersionReq::parse(">=2.30").expect(">=2.30 must parse");
    assert_eq!(req.comparators[0].version.to_string(), "2.30.0");
    assert!(req.matches(&Version::parse("2.30.0").unwrap()));
    assert!(req.matches(&Version::parse("2.31.5").unwrap()));
    assert!(!req.matches(&Version::parse("2.29.9").unwrap()));
}

#[test]
fn compat_pipeline_accepts_reported_shorthand() {
    let manifest = manifest_with_bitty_compat(">=0.5,<1.0");
    assert!(check_compatibility(&manifest, Some("0.6.0"), Some("1.0.0")).is_ok());
    assert!(check_compatibility(&manifest, Some("0.4.9"), Some("1.0.0")).is_err());
    assert!(check_compatibility(&manifest, Some("1.0.0"), Some("1.0.0")).is_err());
}

#[test]
fn caret_u32_max_is_clean_error() {
    for raw in ["^4294967295", "^4294967295.0.0", "^0.4294967295"] {
        let error = VersionReq::parse(raw).expect_err("unrepresentable bound must be rejected");
        assert!(error.to_string().contains("overflow"), "{raw}: {error}");
    }
}

#[test]
fn tilde_minor_u32_max_is_clean_error() {
    let error = VersionReq::parse("~1.4294967295").expect_err("unrepresentable bound rejected");
    assert!(error.to_string().contains("overflow"), "{error}");
}

#[test]
fn maximal_representable_bounds_still_evaluate() {
    let caret = VersionReq::parse("^4294967294").unwrap();
    assert_eq!(caret.comparators[1].version.to_string(), "4294967295.0.0");
    assert!(caret.matches(&Version::parse("4294967294.4294967295.4294967295").unwrap()));
    assert!(!caret.matches(&Version::parse("4294967295.0.0").unwrap()));
}
