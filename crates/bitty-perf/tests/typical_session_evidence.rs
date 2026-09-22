//! Typical-session PB-3 evidence contract (CTX-0676, PERF-04).
//!
//! CI half of the bounded synthetic 8-tab harness. The live measurement needs
//! only `/proc/self` RSS (Linux) and is headless-safe; the committed artifact
//! must stay provenanced and honest. No display, network, clock, RNG, or host
//! path participates; `forbid(unsafe)`.

#![forbid(unsafe_code)]

use bitty_perf::real_window::workspace_root;
use bitty_perf::typical_session::{
    TYPICAL_SESSION_BASELINE_REL_PATH, committed_session_json, growth_pct, meets_pb3,
    meets_reclaim, workload_bytes,
};

#[test]
fn workload_and_math_are_deterministic() {
    assert_eq!(workload_bytes(4096), workload_bytes(4096));
    assert!((growth_pct(100.0, 110.0).unwrap() - 10.0).abs() < 1e-9);
    assert!(growth_pct(0.0, 1.0).is_none());
    assert!(meets_pb3(250.0));
    assert!(!meets_pb3(250.001));
    assert!(meets_reclaim(100.0, 115.0));
    assert!(!meets_reclaim(100.0, 115.001));
}

#[test]
fn committed_baseline_is_provenanced_and_honest() {
    let text = committed_session_json();
    for key in [
        "\"task\"",
        "\"issues\"",
        "\"captured_at\"",
        "\"revision\"",
        "\"command\"",
        "\"host_context\"",
        "\"budget_ref\"",
        "\"session\"",
    ] {
        assert!(text.contains(key), "committed PB-3 baseline missing {key}");
    }
    assert!(
        !text.contains("pending") && !text.contains("unspecified"),
        "committed baseline still carries placeholder provenance"
    );
    assert!(
        !text.contains("/home/") && !text.contains("/mnt/") && !text.contains("C:\\Users"),
        "baseline must not embed a host path"
    );
    assert!(
        text.contains("\"status\": \"measured\"") || text.contains("\"status\": \"unavailable\""),
        "baseline status must be measured or unavailable"
    );
}

#[test]
fn artifact_path_is_repo_relative_and_present() {
    let path = workspace_root().join(TYPICAL_SESSION_BASELINE_REL_PATH);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("baseline artifact {} must exist: {e}", path.display()));
    assert!(text.starts_with('{') && text.trim_end().ends_with('}'));
}
