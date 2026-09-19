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
    AREAS, Engine, MAX_SCENARIOS, Status, generate_summary_json, load_citation_index,
    load_scenario_file, resolve_authority_source, run_oracle, run_scenario, verify_citations,
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
    // Every expectation names a structured authority and a verbatim citation.
    // A scenario with neither would be a self-golden (Bitty's own output
    // recorded as the oracle) and is rejected.
    let report = run_oracle().expect("run oracle");
    for outcome in &report.outcomes {
        assert!(
            !outcome.provenance.authority.trim().is_empty(),
            "{} has an empty authority id",
            outcome.id
        );
        assert!(
            !outcome.provenance.source.trim().is_empty(),
            "{} has an empty authority label",
            outcome.id
        );
        assert!(
            !outcome.provenance.cite.trim().is_empty(),
            "{} has an empty citation token",
            outcome.id
        );
        assert!(
            matches!(
                outcome.provenance.kind,
                bitty_compat_lab::oracle::ProvenanceKind::Spec
                    | bitty_compat_lab::oracle::ProvenanceKind::Capture
            ),
            "{} has an unsupported provenance kind",
            outcome.id
        );
    }
}

#[test]
fn oracle_citations_are_backed_by_their_authority() {
    // F4: the provenance string no longer rubber-stamps a mis-citation. Every
    // scenario's `authority`/`cite` pair must be in the committed index AND,
    // when the authority source is resolvable on this host, the token must
    // appear in that exact source verbatim. This rejects e.g. a ctlseqs
    // citation for DECSET 2026 (which ctlseqs does not define).
    let scenarios = bitty_compat_lab::oracle::load_scenarios().expect("load scenarios");
    assert!(!scenarios.is_empty(), "no scenarios to verify");
    verify_citations(&scenarios).expect("citations must be backed by their authority");

    // The index itself is non-trivial and every entry names a real authority,
    // so a free-form citation cannot enter it.
    let index = load_citation_index().expect("citation index");
    assert!(index.len() >= 20, "citation index suspiciously small");
    for entry in &index {
        bitty_compat_lab::oracle::authority(&entry.authority)
            .unwrap_or_else(|| panic!("index authority {:?} is unknown", entry.authority));
    }
}

#[test]
fn oracle_citation_sources_are_verified_when_present() {
    // Source-verbatim re-verification. The read-only reference snapshot
    // (`recording/references/`) and the `docs/` submodule are absent from a
    // bare CI checkout (the container mounts only the repository, and CI does
    // not init the submodule), so this degrades to an explicit skip there. On a
    // developer/commander checkout it runs and fails on any token the source
    // does not contain. `scripts/gen-oracle-scenarios.sh` additionally greps
    // every token from its source at generation time, so the committed index
    // cannot hold a token that was never in the source.
    let index = load_citation_index().expect("citation index");
    let resolvable: Vec<_> = index
        .iter()
        .filter_map(|entry| {
            let auth = bitty_compat_lab::oracle::authority(&entry.authority)
                .expect("validated index authority");
            resolve_authority_source(auth).map(|path| (entry, path))
        })
        .collect();
    if resolvable.is_empty() {
        eprintln!(
            "SKIP oracle_citation_sources_are_verified_when_present: no authority source \
             present (bare CI checkout); index-membership check still ran"
        );
        return;
    }
    for (entry, path) in &resolvable {
        let text = std::fs::read_to_string(path).expect("read authority source");
        assert!(
            text.contains(&entry.cite),
            "index entry {}/{:?} is not present verbatim in {}",
            entry.authority,
            entry.cite,
            path.display()
        );
    }
}

#[test]
fn oracle_rejects_a_mis_citation() {
    // Direct proof the guard is not vacuous: a fabricated ctlseqs token that
    // does not exist in ctlseqs.txt must fail verification.
    use bitty_compat_lab::oracle::{CiteEntry, verify_citation};
    let index = load_citation_index().expect("citation index");
    let bogus = CiteEntry {
        authority: "xterm-ctlseqs".to_string(),
        cite: "DECSET 2026 -> Enable Synchronized Updates".to_string(),
    };
    assert!(
        !index.contains(&bogus),
        "fabricated citation must not be in the index"
    );
    assert!(
        verify_citation(&index, &bogus.authority, &bogus.cite).is_err(),
        "guard accepted a citation that is not indexed"
    );
    // And a real authority with a token not present in its source fails too.
    if resolve_authority_source(
        bitty_compat_lab::oracle::authority("xterm-ctlseqs").expect("authority"),
    )
    .is_some()
    {
        assert!(
            verify_citation(
                &index,
                "xterm-ctlseqs",
                "Ps = 2 0 2 6  -> Enable Synchronized Updates."
            )
            .is_err(),
            "guard accepted a token absent from ctlseqs.txt"
        );
    }
}

#[test]
fn oracle_covers_both_engines() {
    // The corpus must exercise the runtime engine (F6): OSC 10/11 query
    // response round trip and mouse emitted bytes, not only the parse action.
    let report = run_oracle().expect("run oracle");
    let runtime: Vec<&str> = report
        .outcomes
        .iter()
        .filter(|o| o.engine == Engine::Runtime)
        .map(|o| o.id.as_str())
        .collect();
    assert!(
        runtime.contains(&"osc-10-11-roundtrip"),
        "missing OSC 10/11 runtime round-trip; runtime scenarios: {runtime:?}"
    );
    for id in ["mouse-emit-x10", "mouse-emit-sgr", "mouse-emit-urxvt"] {
        assert!(
            runtime.contains(&id),
            "missing emitted-bytes scenario {id}; runtime scenarios: {runtime:?}"
        );
    }
    // The round-trip asserts a non-empty reply, and the mouse scenarios a
    // non-empty emit, so the runtime path is genuinely observed.
    let round_trip = report
        .outcomes
        .iter()
        .find(|o| o.id == "osc-10-11-roundtrip")
        .expect("round trip");
    assert!(
        round_trip
            .checks
            .iter()
            .any(|c| c.name == "reply" && c.passed),
        "round trip did not pass its reply check"
    );
}

#[test]
fn oracle_summary_is_deterministic_and_bounded() {
    let first = run_oracle().expect("run oracle");
    let second = run_oracle().expect("run oracle second");
    let a = generate_summary_json(&first).expect("summary");
    let b = generate_summary_json(&second).expect("summary second");
    assert_eq!(a, b, "oracle summary must be deterministic");
    assert!(a.len() < 256 * 1024, "summary {} bytes", a.len());
    assert!(a.contains("\"schema_version\": 2"), "missing schema");
    assert!(a.contains("\"summary\":"), "missing summary");
    assert!(a.contains("\"engine\":"), "summary must record the engine");
    assert!(a.contains("\"cite\":"), "summary must record the citation");
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
