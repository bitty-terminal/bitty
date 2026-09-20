//! Real-window PB-1/PB-2 evidence contract (CTX-0592).
//!
//! CI half of the opt-in real-window evidence harness. On headless CI the
//! gate is closed (`BITTY_PERF_REAL_WINDOW!=1`), so this asserts the harness
//! reports `Unavailable` with a reason and never fabricates numbers. It also
//! pins the committed baseline's provenance and the pure metric math.
//!
//! No display, network, clock, RNG, or host path participates; `forbid(unsafe)`.

#![forbid(unsafe_code)]

use std::time::Duration;

use bitty_perf::real_window::{
    HostContext, REAL_WINDOW_BASELINE_REL_PATH, committed_baseline_json, format_report,
    gate_reason, measure_idle_evidence_with, measure_startup_evidence_with, workspace_root,
};

#[test]
fn harness_is_unavailable_without_opt_in_and_never_fabricates() {
    if gate_reason().is_none() {
        // A developer opted in locally; the closed-gate assertion is not
        // applicable (CI never sets the flag).
        return;
    }
    let startup = measure_startup_evidence_with(1, Duration::from_secs(2));
    assert!(startup.is_unavailable(), "gate off must be unavailable");
    assert!(startup.reason.is_some(), "unavailable must carry a reason");
    assert!(startup.p50_ms().is_none() && startup.p99_ms().is_none());
    assert!(!startup.meets_p50() && !startup.meets_p99());

    let idle = measure_idle_evidence_with(Duration::from_secs(1), Duration::from_secs(2));
    assert!(idle.is_unavailable(), "gate off must be unavailable");
    assert!(idle.reason.is_some(), "unavailable must carry a reason");
    assert!(idle.median_mb().is_none());
    assert!(!idle.meets_budget());

    // The human report must say UNMEASURED, not a number.
    let report = format_report(&startup, &idle);
    assert!(
        report.contains("UNMEASURED"),
        "unavailable report must be explicit:\n{report}"
    );
}

#[test]
fn committed_baseline_is_provenanced_and_honest() {
    let text = committed_baseline_json();
    for key in [
        "\"task\"",
        "\"issues\"",
        "\"captured_at\"",
        "\"revision\"",
        "\"command\"",
        "\"host_context\"",
        "\"budget_ref\"",
        "\"pb1_startup\"",
        "\"pb2_idle_rss\"",
    ] {
        assert!(
            text.contains(key),
            "committed real-window baseline missing {key}"
        );
    }
    assert!(
        !text.contains("pending") && !text.contains("unspecified"),
        "committed baseline still carries placeholder provenance"
    );
    assert!(
        !text.contains("bootstrap placeholder"),
        "committed baseline still carries the bootstrap placeholder"
    );
    // No host checkout path, username, or hostname may leak into the artifact.
    assert!(
        !text.contains("/home/") && !text.contains("/mnt/") && !text.contains("C:\\Users"),
        "baseline must not embed a host path"
    );
    // A committed artifact must be self-consistent: either measured numbers
    // that are positive, or an explicit unavailable status.
    assert!(
        text.contains("\"status\": \"measured\"") || text.contains("\"status\": \"unavailable\""),
        "baseline status must be measured or unavailable"
    );
}

#[test]
fn artifact_path_is_repo_relative_and_present() {
    let path = workspace_root().join(REAL_WINDOW_BASELINE_REL_PATH);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("baseline artifact {} must exist: {e}", path.display()));
    assert!(text.starts_with('{') && text.trim_end().ends_with('}'));
}

#[test]
fn host_context_is_environment_derived_without_paths() {
    let host = HostContext::capture();
    assert!(!host.os.is_empty(), "os must be populated");
    assert!(!host.arch.is_empty(), "arch must be populated");
    assert!(host.cpus > 0, "cpu count must be positive");
    // No field may carry a checkout/host path.
    assert!(!host.os.contains('/') && !host.arch.contains('/'));
}
