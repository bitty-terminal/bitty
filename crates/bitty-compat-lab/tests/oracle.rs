#![forbid(unsafe_code)]
//! M1 differential oracle corpus regression (CTX-0573, Issue #1133).
//!
//! Guards the committed oracle corpus: every scenario replays bounded and
//! deterministic through the shared compat-lab harness, every expectation is
//! externally derived (spec citation or reference capture — never Bitty's own
//! output), every declared area is covered, the machine-readable summary is
//! deterministic and bounded, and a deliberately divergent expectation is
//! caught (differential-power proof).

use std::path::PathBuf;

use bitty_compat_lab::oracle::{
    AREAS, MAX_SCENARIOS, ProvenanceKind, Status, generate_summary_json, load_scenario_file,
    run_oracle, run_scenario,
};

fn divergence_dir() -> PathBuf {
    bitty_compat_lab::workspace_root().join("tests/compat/oracle/divergences")
}

#[test]
fn oracle_corpus_covers_every_declared_area() {
    let report = run_oracle().expect("run oracle");
    assert!(!report.outcomes.is_empty(), "oracle corpus is empty");
    assert!(
        report.outcomes.len() <= MAX_SCENARIOS,
        "oracle scenarios {} > MAX_SCENARIOS",
        report.outcomes.len()
    );
    for area in AREAS {
        assert!(
            report.outcomes.iter().any(|o| o.area == *area),
            "oracle corpus has no scenario for area {area:?}"
        );
    }
    // Areas are unique and in the declared priority order.
    let mut seen = std::collections::BTreeSet::new();
    for area in AREAS {
        assert!(seen.insert(*area), "duplicate oracle area {area:?}");
    }
}

#[test]
fn oracle_corpus_is_green_against_the_bitty_build() {
    let report = run_oracle().expect("run oracle");
    assert!(
        report.all_passed(),
        "oracle divergences:\n{}",
        render_failures(&report)
    );
    assert_eq!(report.passed, report.total());
    assert_eq!(report.failed, 0);
}

#[test]
fn oracle_expectations_are_externally_derived_not_self_golden() {
    // Every expectation names a spec citation or a captured reference
    // terminal. A scenario with neither would be a self-golden (Bitty's own
    // output recorded as the oracle) and is rejected.
    let report = run_oracle().expect("run oracle");
    for outcome in &report.outcomes {
        assert!(
            !outcome.provenance.source.trim().is_empty(),
            "{} has an empty provenance source",
            outcome.id
        );
        assert!(
            matches!(
                outcome.provenance.kind,
                ProvenanceKind::Spec | ProvenanceKind::Capture
            ),
            "{} has an unsupported provenance kind",
            outcome.id
        );
        // Spec citations must name the authoritative control-sequence source;
        // capture citations must name a terminal.
        let source = outcome.provenance.source.to_lowercase();
        match outcome.provenance.kind {
            ProvenanceKind::Spec => assert!(
                source.contains("ctlseqs")
                    || source.contains("spec")
                    || source.contains("m1 rfc")
                    || source.contains("ecma"),
                "{} spec provenance does not cite an authoritative source: {}",
                outcome.id,
                outcome.provenance.source
            ),
            ProvenanceKind::Capture => assert!(
                source.contains("xterm")
                    || source.contains("ghostty")
                    || source.contains("kitty")
                    || source.contains("wezterm")
                    || source.contains("alacritty"),
                "{} capture provenance does not name a reference terminal: {}",
                outcome.id,
                outcome.provenance.source
            ),
        }
    }
}

#[test]
fn oracle_summary_is_deterministic_and_bounded() {
    let first = run_oracle().expect("run oracle");
    let second = run_oracle().expect("run oracle second");
    let a = generate_summary_json(&first).expect("summary");
    let b = generate_summary_json(&second).expect("summary second");
    assert_eq!(a, b, "oracle summary must be deterministic");
    assert!(a.len() < 256 * 1024, "summary {} bytes", a.len());
    assert!(a.contains("\"schema_version\": 1"), "missing schema");
    assert!(a.contains("\"summary\":"), "missing summary");
    assert!(
        !a.contains("winit") && !a.contains("wgpu"),
        "summary must not reference window/GPU backends"
    );
    // Every scenario appears with a pass/fail status.
    for outcome in &first.outcomes {
        let needle = format!("\"id\": \"{}\"", outcome.id);
        assert!(a.contains(&needle), "summary missing scenario {needle:?}");
    }
}

#[test]
fn oracle_corpus_is_bounded_and_deterministic() {
    let report = run_oracle().expect("run oracle");
    for outcome in &report.outcomes {
        // Re-running one scenario twice yields identical checks.
        let bin = bitty_compat_lab::workspace_root()
            .join("tests/compat/oracle/scenarios")
            .join(format!("{}.bin", outcome.id));
        let scenario = load_scenario_file(&bin).expect("load scenario");
        let one = run_scenario(&scenario);
        let two = run_scenario(&scenario);
        assert_eq!(one, two, "{} is not deterministic", outcome.id);
        assert!(
            scenario.corpus.len() <= bitty_compat_lab::MAX_CORPUS_BYTES,
            "{} corpus exceeds MAX_CORPUS_BYTES",
            outcome.id
        );
        // Snapshot replay asserts the 80x24 canonical grid via check_grid.
        assert!(
            one.checks.iter().any(|c| c.name == "grid"),
            "{} did not check the grid",
            outcome.id
        );
    }
}

#[test]
fn oracle_runner_catches_every_deliberate_divergence() {
    // Differential-power proof. Each fixture under `divergences/` carries an
    // oracle whose expectation disagrees with the build. If the runner ever
    // rubber-stamped Bitty's output, these oracles would pass and this test
    // would fail.
    let dir = divergence_dir();
    let mut fixtures: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read divergence dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("bin"))
        .collect();
    fixtures.sort();
    assert!(
        fixtures.len() >= 2,
        "need at least two divergence fixtures (oracle-wrong and bitty-wrong): {fixtures:?}"
    );
    for bin in &fixtures {
        let scenario = load_scenario_file(bin).expect("load divergence fixture");
        let outcome = run_scenario(&scenario);
        assert_eq!(
            outcome.status,
            Status::Fail,
            "runner failed to catch divergence fixture {:?}: {outcome:?}",
            bin.file_name()
        );
        assert!(
            outcome.checks.iter().any(|c| !c.passed),
            "divergence fixture {:?} produced no failing check",
            bin.file_name()
        );
    }

    // Direction 1 — oracle wrong, Bitty right. This fixture records the
    // pre-CTX-0175 wrong reading of mode 1007 (`focus_events`); the build
    // correctly reports `alternate_scroll`.
    let wrong_oracle =
        load_scenario_file(&dir.join("mouse-1007-misclassified.bin")).expect("load fixture");
    let outcome = run_scenario(&wrong_oracle);
    let mode_check = outcome
        .checks
        .iter()
        .find(|c| c.name == "mode focus_events")
        .expect("fixture must check the misclassified mode");
    assert_eq!(mode_check.expected, "on");
    assert_eq!(mode_check.actual, "off");

    // Direction 2 — Bitty diverges, spec-derived oracle right. Keep the
    // spec-derived oracle for `sync-2026-set` and mutate the input so Bitty's
    // observed state diverges from it. DECRQM `CSI ? 2026 $ p` is a query and
    // does not set the mode, so Bitty observes `off` while the spec oracle
    // expects `on`; the runner must report FAIL.
    let spec_bin =
        bitty_compat_lab::workspace_root().join("tests/compat/oracle/scenarios/sync-2026-set.bin");
    let mut mutated = load_scenario_file(&spec_bin).expect("load sync-2026-set");
    mutated.corpus = b"\x1b[?2026$p".to_vec();
    let mutated_outcome = run_scenario(&mutated);
    assert_eq!(
        mutated_outcome.status,
        Status::Fail,
        "runner failed to catch a Bitty-side divergence from the spec oracle: {mutated_outcome:?}"
    );
    let sync_check = mutated_outcome
        .checks
        .iter()
        .find(|c| c.name == "mode synchronized_update")
        .expect("sync oracle must check the mode");
    assert!(!sync_check.passed, "sync divergence check passed");
    assert_eq!(sync_check.expected, "on");
    assert_eq!(sync_check.actual, "off");
}

fn render_failures(report: &bitty_compat_lab::oracle::OracleReport) -> String {
    let mut out = String::new();
    for outcome in &report.outcomes {
        if outcome.status != Status::Fail {
            continue;
        }
        out.push_str(&format!("{} [{}]:\n", outcome.id, outcome.area));
        for check in outcome.checks.iter().filter(|c| !c.passed) {
            out.push_str(&format!(
                "  {}: expected {} got {}\n",
                check.name, check.expected, check.actual
            ));
        }
    }
    out
}
