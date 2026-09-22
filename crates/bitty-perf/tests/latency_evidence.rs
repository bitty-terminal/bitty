//! Input-latency PB-4 evidence contract (CTX-0686, PERF-05).
//!
//! CI half of the headless key-to-screen harness. The live measurement needs
//! no display (software present seam); the committed artifact must stay
//! provenanced and honest. No network, clock, RNG, or host path
//! participates; `forbid(unsafe)`.

#![forbid(unsafe_code)]

use std::time::Duration;

use bitty_perf::latency::{
    LATENCY_BASELINE_REL_PATH, LatencyMode, LatencyReport, LatencySample, baseline_json,
    committed_latency_json, measure_latency,
};
use bitty_perf::real_window::{BaselineMeta, workspace_root};

fn sample(total_ms: f64, work_ms: f64) -> LatencySample {
    LatencySample {
        total: Duration::from_secs_f64(total_ms / 1000.0),
        encode: Duration::from_micros(100),
        handle_key: Duration::ZERO,
        pty_to_state: Duration::ZERO,
        render_present: Duration::from_secs_f64(work_ms / 1000.0),
        presented: true,
        is_synthetic: true,
    }
}

fn report_at(wall_ms: f64, work_ms: f64) -> LatencyReport {
    let samples = vec![sample(wall_ms, work_ms); 200];
    LatencyReport {
        samples,
        p50_ms: wall_ms,
        p99_ms: wall_ms,
        mean_ms: wall_ms,
        max_ms: wall_ms,
        p50_work_ms: work_ms,
        p99_work_ms: work_ms,
        min_work_ms: work_ms,
        mode: LatencyMode::InjectedEcho,
        headless: true,
        idle_misses: 0,
        synthetic_samples: 200,
    }
}

fn test_meta() -> BaselineMeta {
    BaselineMeta {
        task: "CTX-0686".to_string(),
        issues: vec![1059],
        captured_at: "2026-09-22".to_string(),
        revision: "test-revision".to_string(),
        command: "test command".to_string(),
        profile: "test".to_string(),
    }
}

#[test]
fn budget_predicates_are_exact_at_the_pb4_bounds() {
    let at_p50 = report_at(8.0, 8.0);
    assert!(at_p50.meets_p50());
    assert!(at_p50.meets_work_p50());

    let over_p50 = report_at(8.001, 8.001);
    assert!(!over_p50.meets_p50());
    assert!(!over_p50.meets_work_p50());

    let at_p99 = report_at(8.0, 15.0);
    assert!(at_p99.meets_p99());
    assert!(at_p99.meets_work_p99());

    let over_p99 = report_at(8.0, 15.001);
    assert!(over_p99.meets_p99());
    assert!(!over_p99.meets_work_p99());
}

#[test]
fn baseline_json_refuses_zero_presented_samples() {
    let mut empty = report_at(1.0, 1.0);
    empty.samples = Vec::new();
    empty.idle_misses = 200;
    assert!(baseline_json(&empty, &test_meta()).is_err());
}

#[test]
fn baseline_json_carries_schema_budget_and_numbers() {
    let json = baseline_json(&report_at(1.0, 1.0), &test_meta()).expect("measured serializes");
    for needle in [
        "\"schema_version\": 1",
        "\"task\": \"CTX-0686\"",
        "\"issues\": [1059]",
        "pb-4",
        "\"budget_p50_ms\": 8",
        "\"budget_p99_ms\": 15",
        "\"meets_work_p50\": true",
        "\"mode\": \"injected-echo\"",
    ] {
        assert!(json.contains(needle), "baseline must contain {needle}");
    }
}

#[test]
fn small_sample_measures_headlessly() {
    let report = measure_latency(8);
    assert_eq!(report.samples.len(), 8);
    let presented = report.samples.iter().filter(|s| s.presented).count();
    assert!(
        presented >= report.samples.len() / 2,
        "presented {presented}/8 must be >= half"
    );
    assert!(report.format_summary().contains("mode="));
}

#[test]
fn committed_baseline_is_provenanced_and_honest() {
    let text = committed_latency_json();
    for key in [
        "\"task\"",
        "\"issues\"",
        "\"captured_at\"",
        "\"revision\"",
        "\"command\"",
        "\"host_context\"",
        "\"budget_ref\"",
        "\"latency\"",
    ] {
        assert!(text.contains(key), "committed PB-4 baseline missing {key}");
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
        "committed PB-4 baseline must be a measured capture"
    );
}

#[test]
fn artifact_path_is_repo_relative_and_present() {
    let path = workspace_root().join(LATENCY_BASELINE_REL_PATH);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("baseline artifact {} must exist: {e}", path.display()));
    assert!(text.starts_with('{') && text.trim_end().ends_with('}'));
}
