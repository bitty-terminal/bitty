//! Continuous daily-driver dogfood session contract (CTX-0643, PERF-10).
//!
//! CI half of the opt-in session automation. On headless CI the gate is
//! closed (`BITTY_PERF_DOGFOOD_SESSION!=1`) and no Hyprland session exists,
//! so this asserts the planner reports `Unavailable`/`UNAVAILABLE` reasons,
//! the cycle plan math holds, the app selection stays validated, and the
//! committed artifact stays provenanced and honest. No display, network,
//! clock, RNG, or host path participates; `forbid(unsafe)`.

#![forbid(unsafe_code)]

use bitty_perf::dogfood_session::{
    DOGFOOD_SESSION_BASELINE_REL_PATH, MAX_CYCLES, SESSION_APPS, SessionConfig, SessionCycle,
    committed_session_json, format_session_report, plan_cycles, session_evidence_json,
    session_gate_reason, session_schedule_json, validated_session_apps,
};
use bitty_perf::real_soak::{capture_leg_status, rss_growth_pct, rss_trend};
use bitty_perf::real_window::{BaselineMeta, workspace_root};

fn test_meta() -> BaselineMeta {
    BaselineMeta {
        task: "CTX-0643".to_string(),
        issues: vec![1064],
        captured_at: "2026-09-22".to_string(),
        revision: "test-revision".to_string(),
        command: "bash scripts/dogfood-session.sh --dry-run".to_string(),
        profile: "test".to_string(),
    }
}

#[test]
fn planner_is_unavailable_without_opt_in_and_never_fabricates() {
    // Planning itself is pure and headless-safe, but the gate and the live
    // leg must both report unavailable on CI. On a Tier 1 developer host
    // the leg may genuinely be live; like the soak contract, the closed-leg
    // assertion is then not applicable (CI never has either).
    let leg = capture_leg_status();
    if session_gate_reason().is_none() || leg.available {
        return;
    }
    assert!(leg.reason.is_some(), "unavailable leg needs a reason");

    // An empty evidence set serializes as unavailable, never measured.
    let config = SessionConfig::from_env();
    let plan = plan_cycles(config.duration_secs, config.cycle_secs);
    let json = session_evidence_json(
        &config,
        &plan,
        &[],
        None,
        Some("gate closed: BITTY_PERF_DOGFOOD_SESSION!=1"),
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

    // The human report must say UNMEASURED, not a number.
    let report = format_session_report(
        &config,
        &plan,
        &leg,
        Some("gate closed: BITTY_PERF_DOGFOOD_SESSION!=1"),
    );
    assert!(
        report.contains("UNMEASURED"),
        "unavailable report must be explicit:\n{report}"
    );
}

#[test]
fn default_plan_matches_the_pb3_four_hour_window() {
    let plan = plan_cycles(14_400, 600);
    assert_eq!(plan.duration_secs, 14_400);
    assert_eq!(plan.cycles, 25);
    assert_eq!(plan.cycle_at_secs[0], 0);
    assert_eq!(plan.cycle_at_secs[plan.cycles - 1], 14_400);
}

#[test]
fn plan_never_exceeds_max_cycles() {
    let plan = plan_cycles(u64::MAX, 1);
    assert!(plan.cycles <= MAX_CYCLES);
    assert_eq!(plan.cycle_at_secs.len(), plan.cycles);
    // Offsets are strictly increasing from zero.
    let mut sorted = plan.cycle_at_secs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), plan.cycles);
    assert_eq!(plan.cycle_at_secs[0], 0);
}

#[test]
fn schedule_json_is_provenanced_and_portable() {
    let config = SessionConfig::from_env();
    let plan = plan_cycles(600, 300);
    let json = session_schedule_json(&config, &plan, &test_meta());
    for key in [
        "\"task\": \"CTX-0643\"",
        "\"issues\": [1064]",
        "\"captured_at\"",
        "\"revision\"",
        "\"command\"",
        "\"host_context\"",
        "\"budget_ref\"",
        "\"planned_cycles\": 3",
        "\"status\": \"scheduled\"",
        "\"apps\": [\"shell\", \"cargo\", \"git\", \"nvim\", \"tmux\", \"ssh\"]",
    ] {
        assert!(json.contains(key), "schedule json missing {key}:\n{json}");
    }
    assert!(
        !json.contains("/home/") && !json.contains("/mnt/") && !json.contains("C:\\\\Users"),
        "schedule must not embed a host path:\n{json}"
    );
}

#[test]
fn evidence_json_records_basenames_only() {
    let config = SessionConfig::from_env();
    let plan = plan_cycles(600, 300);
    let cycles = vec![
        SessionCycle {
            index: 0,
            at_secs: 0,
            screenshot: "recording/dogfood-session/cycle-0000.png".to_string(),
            rss_mb: Some(400.0),
            grid_text_bytes: Some(1024),
            apps_driven: 6,
            driver_ok: true,
        },
        SessionCycle {
            index: 1,
            at_secs: 300,
            screenshot: "cycle-0001.png".to_string(),
            rss_mb: Some(405.5),
            grid_text_bytes: None,
            apps_driven: 4,
            driver_ok: false,
        },
    ];
    let trend = rss_trend(&[400.0, 405.5]);
    let json = session_evidence_json(&config, &plan, &cycles, trend, None, &test_meta());
    assert!(
        json.contains("\"status\": \"measured\""),
        "completed evidence must be measured:\n{json}"
    );
    assert!(
        json.contains("\"screenshot\": \"cycle-0000.png\""),
        "screenshots must be basenames:\n{json}"
    );
    assert!(
        !json.contains("recording/dogfood-session/cycle"),
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
    assert!(
        json.contains("\"apps_driven\": 6"),
        "measured evidence must carry the per-cycle ledger:\n{json}"
    );
}

#[test]
fn committed_session_baseline_is_provenanced_and_honest() {
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
        assert!(
            text.contains(key),
            "committed session baseline missing {key}"
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
    assert!(
        !text.contains("/home/") && !text.contains("/mnt/") && !text.contains("C:\\\\Users"),
        "baseline must not embed a host path"
    );
    assert!(
        text.contains("\"status\": \"measured\"") || text.contains("\"status\": \"unavailable\""),
        "baseline status must be measured or unavailable"
    );
}

#[test]
fn artifact_path_is_repo_relative_and_present() {
    let path = workspace_root().join(DOGFOOD_SESSION_BASELINE_REL_PATH);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("baseline artifact {} must exist: {e}", path.display()));
    assert!(text.starts_with('{') && text.trim_end().ends_with('}'));
}

#[test]
fn app_selection_stays_validated_and_canonical() {
    // The full set passes through in canonical order regardless of input order.
    assert_eq!(
        validated_session_apps("ssh,shell,cargo,git,nvim,tmux"),
        SESSION_APPS
            .iter()
            .map(|app| (*app).to_string())
            .collect::<Vec<_>>(),
    );
    // Case-insensitive, whitespace-tolerant, canonical order.
    assert_eq!(validated_session_apps("  NVIM , Ssh "), vec!["nvim", "ssh"]);
    // Duplicates collapse to one entry.
    assert_eq!(validated_session_apps("git,git,git"), vec!["git"]);
    // Unknown names never re-target the driver; empty falls back to all six.
    assert_eq!(
        validated_session_apps("rm -rf /"),
        SESSION_APPS
            .iter()
            .map(|app| (*app).to_string())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        validated_session_apps(""),
        SESSION_APPS
            .iter()
            .map(|app| (*app).to_string())
            .collect::<Vec<_>>()
    );
    // Cycle basename keeps file names only (drives the script's evidence shape).
    assert_eq!(
        SessionCycle::screenshot_basename("a/b/cycle-0001.png"),
        "cycle-0001.png"
    );
    // RSS helpers are shared with the soak chain: empty series is
    // unavailable, growth math is exact.
    assert_eq!(rss_trend(&[]), None);
    let trend = rss_trend(&[200.0, 250.0]).expect("trend");
    assert!((rss_growth_pct(&trend) - 25.0).abs() < f64::EPSILON);
}
