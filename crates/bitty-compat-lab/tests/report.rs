#![forbid(unsafe_code)]
//! M1/M2 compatibility report regression (CTX-0404).
//!
//! Guards the machine-readable report emitted by
//! `bitty-compat-lab::report::generate_report_json` and the
//! `compat_report` binary: full area coverage, honest per-status methods,
//! deterministic bounded JSON, and env-gated local rows that map onto real
//! `tests/live_compat.rs` scenario tests.

use bitty_compat_lab::report::{
    AREAS, MAX_REPORT_BYTES, PROBED_TOOLS, ROWS, Status, generate_report_json,
};

fn row_count(status: Status) -> usize {
    ROWS.iter().filter(|row| row.status == status).count()
}

#[test]
fn report_covers_all_declared_areas_in_priority_order() {
    assert!(!AREAS.is_empty(), "areas must not be empty");
    for (idx, area) in AREAS.iter().enumerate() {
        let rows = ROWS.iter().filter(|row| row.area == *area).count();
        assert!(rows > 0, "area {area:?} has no rows");
        assert_eq!(
            idx,
            AREAS.iter().position(|a| a == area).unwrap(),
            "area {area:?} duplicated"
        );
    }
    // Rows appear in area priority order (author order within an area).
    let mut last = 0usize;
    for row in ROWS {
        let pos = AREAS
            .iter()
            .position(|area| *area == row.area)
            .unwrap_or_else(|| panic!("unknown area {:?}", row.area));
        assert!(
            pos >= last,
            "row {:?}/{:?} breaks area priority order",
            row.area,
            row.scenario
        );
        last = pos;
    }
}

#[test]
fn report_statuses_declare_consistent_methods() {
    use bitty_compat_lab::report::Method;
    for row in ROWS {
        match (row.status, row.method) {
            (Status::Ci, Method::Corpus { .. } | Method::Test { .. }) => {}
            (Status::Local, Method::LocalPty { .. } | Method::LiveDisplay { .. }) => {}
            (Status::Partial, Method::Corpus { .. }) => {}
            (Status::Gap, Method::Uncovered { .. }) => {}
            (status, method) => panic!(
                "row {:?}/{:?} has inconsistent status {:?} and method {method:?}",
                row.area, row.scenario, status
            ),
        }
    }
    assert!(row_count(Status::Ci) > 0, "need ci rows");
    assert!(row_count(Status::Local) > 0, "need local rows");
    assert!(row_count(Status::Partial) > 0, "need partial rows");
    assert!(row_count(Status::Gap) > 0, "need gap rows");
}

#[test]
fn report_json_is_deterministic_and_bounded() {
    let first = generate_report_json().expect("report json");
    let second = generate_report_json().expect("report json second");
    assert_eq!(first, second, "report json must be deterministic");
    assert!(
        first.len() <= MAX_REPORT_BYTES,
        "report {} > MAX_REPORT_BYTES {MAX_REPORT_BYTES}",
        first.len()
    );
    assert!(first.contains("\"schema_version\": 1"), "missing schema");
    assert!(first.contains("\"summary\":"), "missing summary");
    assert!(
        !first.contains("winit") && !first.contains("wgpu"),
        "report must not reference window/GPU backends"
    );
}

#[test]
fn report_summary_matches_row_statuses() {
    let json = generate_report_json().expect("report json");
    for (status, key) in [
        (Status::Ci, "ci"),
        (Status::Local, "local"),
        (Status::Partial, "partial"),
        (Status::Gap, "gap"),
    ] {
        let needle = format!("\"{key}\": {}", row_count(status));
        assert!(json.contains(&needle), "summary missing {needle:?}\n{json}");
    }
}

#[test]
fn report_environment_probe_is_sorted_and_complete() {
    assert!(
        PROBED_TOOLS.windows(2).all(|w| w[0] < w[1]),
        "PROBED_TOOLS must be sorted: {PROBED_TOOLS:?}"
    );
    let json = generate_report_json().expect("report json");
    for tool in PROBED_TOOLS {
        let key = format!("\"{tool}\": ");
        assert!(json.contains(&key), "environment probe missing {tool:?}");
    }
}

#[test]
fn report_local_rows_map_to_live_scenario_tests() {
    use bitty_compat_lab::report::Method;
    let live = std::fs::read_to_string(
        bitty_compat_lab::workspace_root().join("crates/bitty-compat-lab/tests/live_compat.rs"),
    )
    .expect("read live_compat.rs");
    let clipboard_live = std::fs::read_to_string(
        bitty_compat_lab::workspace_root().join("crates/bitty-platform/tests/clipboard_live.rs"),
    )
    .expect("read clipboard_live.rs");
    for row in ROWS {
        if let Method::LocalPty { scenario } = row.method {
            let needle = format!("fn live_{scenario}_probe(");
            assert!(
                live.contains(&needle),
                "live scenario {scenario:?} for {:?}/{:?} has no {needle}",
                row.area,
                row.scenario
            );
        }
        if let Method::LiveDisplay { scenario } = row.method {
            // Every live-display scenario owns at least one
            // `live_{scenario}_*` test in `clipboard_live.rs`.
            let needle = format!("fn live_{scenario}_");
            assert!(
                clipboard_live.contains(&needle),
                "live-display scenario {scenario:?} for {:?}/{:?} has no {needle} test",
                row.area,
                row.scenario
            );
        }
    }
}

#[test]
fn report_ci_rows_verify_and_emit_evidence() {
    let json = generate_report_json().expect("report json");
    assert!(
        json.contains("\"check\": \"bounded-deterministic-invariants\""),
        "corpus evidence check missing"
    );
    assert!(
        json.contains("\"check\": \"test-present\""),
        "test evidence check missing"
    );
    assert!(
        json.contains("\"check\": \"env-gated-BITTY_COMPAT_LIVE\""),
        "local evidence check missing"
    );
    assert!(
        json.contains("\"evidence_check\": \"uncovered\""),
        "gap evidence check missing"
    );
}
