//! Long-duration real-render soak contract (CTX-0642, PERF-09).
//!
//! CI half of the opt-in soak automation. On headless CI the gate is closed
//! (`BITTY_PERF_REAL_SOAK!=1`) and no Hyprland session exists, so this
//! asserts the planner reports `Unavailable`/`UNAVAILABLE` reasons, the
//! capture plan math holds, and the committed artifact stays provenanced and
//! honest. No display, network, clock, RNG, or host path participates;
//! `forbid(unsafe)`.

#![forbid(unsafe_code)]

use bitty_perf::real_soak::{
    CaptureLegStatus, DEFAULT_CAPTURE_INTERVAL_SECS, DEFAULT_DURATION_SECS, MAX_CAPTURES,
    REAL_SOAK_BASELINE_REL_PATH, SoakCapture, SoakConfig, basename, bravais_workspace,
    capture_leg_status, committed_soak_json, evidence_json, meets_pb3, plan_captures,
    resolve_out_dir, rss_growth_pct, rss_trend, schedule_json, soak_gate_reason,
    validated_workload,
};
use bitty_perf::real_window::{BaselineMeta, workspace_root};

fn test_meta() -> BaselineMeta {
    BaselineMeta {
        task: "CTX-0642".to_string(),
        issues: vec![1063],
        captured_at: "2026-09-22".to_string(),
        revision: "test-revision".to_string(),
        command: "bash scripts/real-render-soak.sh --dry-run".to_string(),
        profile: "test".to_string(),
    }
}

#[test]
fn planner_is_unavailable_without_opt_in_and_never_fabricates() {
    // Planning itself is pure and headless-safe, but the gate and the live
    // leg must both report unavailable on CI. On a Tier 1 developer host
    // the leg may genuinely be live; like the real-window contract, the
    // closed-leg assertion is then not applicable (CI never has either).
    let leg = capture_leg_status();
    if soak_gate_reason().is_none() || leg.available {
        return;
    }
    assert!(leg.reason.is_some(), "unavailable leg needs a reason");

    // An empty evidence set serializes as unavailable, never measured.
    let config = SoakConfig::from_env();
    let plan = plan_captures(config.duration_secs, config.interval_secs);
    let json = evidence_json(
        &config,
        &plan,
        &[],
        None,
        Some("gate closed: BITTY_PERF_REAL_SOAK!=1"),
        &test_meta(),
    );
    assert!(
        json.contains("\"status\": \"unavailable\""),
        "empty evidence must be unavailable:\n{json}"
    );
    assert!(
        !json.contains("\"status\": \"measured\""),
        "empty evidence must not claim measured:\n{json}"
    );
}

#[test]
fn default_plan_matches_the_pb3_four_hour_window() {
    let plan = plan_captures(DEFAULT_DURATION_SECS, DEFAULT_CAPTURE_INTERVAL_SECS);
    assert_eq!(plan.duration_secs, DEFAULT_DURATION_SECS);
    assert_eq!(plan.captures, 49);
    assert_eq!(plan.capture_at_secs[0], 0);
    assert_eq!(
        plan.capture_at_secs[plan.captures - 1],
        DEFAULT_DURATION_SECS
    );
}

#[test]
fn plan_never_exceeds_max_captures() {
    let plan = plan_captures(u64::MAX, 1);
    assert!(plan.captures <= MAX_CAPTURES);
    assert_eq!(plan.capture_at_secs.len(), plan.captures);
    // Offsets are strictly increasing from zero.
    let mut sorted = plan.capture_at_secs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), plan.captures);
    assert_eq!(plan.capture_at_secs[0], 0);
}

#[test]
fn schedule_json_is_provenanced_and_portable() {
    let config = SoakConfig::from_env();
    let plan = plan_captures(600, 300);
    let json = schedule_json(&config, &plan, &test_meta());
    for key in [
        "\"task\": \"CTX-0642\"",
        "\"issues\": [1063]",
        "\"captured_at\"",
        "\"revision\"",
        "\"command\"",
        "\"host_context\"",
        "\"budget_ref\"",
        "\"planned_captures\": 3",
        "\"status\": \"scheduled\"",
    ] {
        assert!(json.contains(key), "schedule json missing {key}:\n{json}");
    }
    assert!(
        !json.contains("/home/") && !json.contains("/mnt/") && !json.contains("C:\\Users"),
        "schedule must not embed a host path:\n{json}"
    );
}

#[test]
fn evidence_json_records_basenames_only() {
    let config = SoakConfig::from_env();
    let plan = plan_captures(600, 300);
    let captures = vec![
        SoakCapture {
            index: 0,
            at_secs: 0,
            screenshot: "recording/real-soak/capture-0000.png".to_string(),
            rss_mb: Some(400.0),
            grid_text_bytes: Some(1024),
            driver_ok: true,
        },
        SoakCapture {
            index: 1,
            at_secs: 300,
            screenshot: "capture-0001.png".to_string(),
            rss_mb: Some(405.5),
            grid_text_bytes: None,
            driver_ok: false,
        },
    ];
    let trend = rss_trend(&[400.0, 405.5]);
    let json = evidence_json(&config, &plan, &captures, trend, None, &test_meta());
    assert!(
        json.contains("\"status\": \"measured\""),
        "completed evidence must be measured:\n{json}"
    );
    assert!(
        json.contains("\"screenshot\": \"capture-0000.png\""),
        "screenshots must be basenames:\n{json}"
    );
    assert!(
        !json.contains("recording/real-soak/capture"),
        "evidence must not embed the output dir:\n{json}"
    );
    assert!(
        !json.contains("/home/") && !json.contains("/mnt/"),
        "evidence must not embed a host path:\n{json}"
    );
    assert!(
        json.contains("\"rss_growth_pct\""),
        "measured evidence must carry the RSS trend:\n{json}"
    );
}

#[test]
fn committed_soak_baseline_is_provenanced_and_honest() {
    let text = committed_soak_json();
    for key in [
        "\"task\"",
        "\"issues\"",
        "\"captured_at\"",
        "\"revision\"",
        "\"command\"",
        "\"host_context\"",
        "\"budget_ref\"",
        "\"soak\"",
    ] {
        assert!(text.contains(key), "committed soak baseline missing {key}");
    }
    assert!(
        !text.contains("pending") && !text.contains("unspecified"),
        "committed baseline still carries placeholder provenance"
    );
    assert!(
        !text.contains("bootstrap placeholder"),
        "committed baseline still carries the bootstrap placeholder"
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
    let path = workspace_root().join(REAL_SOAK_BASELINE_REL_PATH);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("baseline artifact {} must exist: {e}", path.display()));
    assert!(text.starts_with('{') && text.trim_end().ends_with('}'));
}

#[test]
fn config_parsing_stays_clamped_and_portable() {
    // Pure validators: unknown inputs fall back, never panic, never leak.
    assert_eq!(validated_workload("idle"), "idle");
    assert_eq!(validated_workload("mixed"), "mixed");
    assert_eq!(validated_workload("input-spam"), "input-spam");
    assert_eq!(validated_workload("rm -rf /"), "mixed");
    assert_eq!(bravais_workspace("4"), "4");
    assert_eq!(bravais_workspace("0"), "4");
    assert_eq!(bravais_workspace("../../../etc"), "4");
    assert_eq!(basename("a/b/capture-0001.png"), "capture-0001.png");
    // Relative out dirs resolve under the workspace root, never absolute
    // from the environment by accident.
    let resolved = resolve_out_dir("recording/real-soak");
    assert_eq!(resolved, workspace_root().join("recording/real-soak"));
    // RSS helpers: empty series is unavailable, PB-3 verdict uses the
    // accepted constant.
    assert_eq!(rss_trend(&[]), None);
    let trend = rss_trend(&[200.0, 250.0]).expect("trend");
    assert!((rss_growth_pct(&trend) - 25.0).abs() < f64::EPSILON);
    assert!(meets_pb3(250.0));
    assert!(!meets_pb3(250.5));
    // Leg probe reports missing tools by name (drives the script's
    // "install hyprctl/grim/jq" hint).
    let leg = CaptureLegStatus::probe(&[("hyprctl", false)], true);
    assert!(!leg.available);
    assert_eq!(leg.missing_tools, vec!["hyprctl"]);
}
