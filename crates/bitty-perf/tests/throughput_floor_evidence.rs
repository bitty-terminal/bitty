//! Throughput-floor PB-6 evidence contract (CTX-0676, PERF-07).
//!
//! CI half of the fixed-corpus parse-and-render harness. The corpus is built
//! in code (deterministic, no file discovery); the committed artifact must
//! stay provenanced and honest. A small live sample proves the path measures
//! headlessly. No display, network, RNG, or host path participates;
//! `forbid(unsafe)`.

#![forbid(unsafe_code)]

use bitty_perf::real_window::workspace_root;
use bitty_perf::throughput_floor::{
    THROUGHPUT_FLOOR_BASELINE_REL_PATH, committed_baseline_json, corpus_segment, mb_per_s, measure,
    median, meets_pb6,
};

#[test]
fn corpus_and_math_are_deterministic() {
    assert_eq!(corpus_segment(), corpus_segment());
    assert!((mb_per_s(1024 * 1024, 1.0).unwrap() - 1.0).abs() < 1e-9);
    assert!(mb_per_s(1024, 0.0).is_none());
    assert!((median(vec![3.0, 1.0, 2.0]).unwrap() - 2.0).abs() < 1e-9);
    assert!(median(Vec::new()).is_none());
    assert!(meets_pb6(40.0));
    assert!(!meets_pb6(39.99));
}

#[test]
fn small_sample_measures_headlessly() {
    let report = measure(64 * 1024, 1).expect("small sample must measure headlessly");
    assert!(report.median_mb_s > 0.0);
    assert!(report.actions_final_round > 0);
    assert!(report.renders_final_round > 0);
    assert!(report.format_summary().contains("MiB/s"));
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
        "\"throughput\"",
    ] {
        assert!(text.contains(key), "committed PB-6 baseline missing {key}");
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
        text.contains("\"status\": \"measured\""),
        "committed PB-6 baseline must be a measured capture"
    );
}

#[test]
fn artifact_path_is_repo_relative_and_present() {
    let path = workspace_root().join(THROUGHPUT_FLOOR_BASELINE_REL_PATH);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("baseline artifact {} must exist: {e}", path.display()));
    assert!(text.starts_with('{') && text.trim_end().ends_with('}'));
}
