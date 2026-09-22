//! Harness-pin PERF-01 contract (CTX-0686).
//!
//! CI half of the PB harness/corpora/machines pin record. Keeps the inventory
//! honest: the pin doc must name every committed artifact, and every named
//! artifact must exist with schema and provenance intact. No display,
//! network, clock, RNG, or host path participates; `forbid(unsafe)`.

#![forbid(unsafe_code)]

use bitty_perf::real_window::workspace_root;

/// Committed PB artifacts pinned by `baselines/harness-pin.md`.
const PINNED_ARTIFACTS: &[&str] = &[
    "crates/bitty-perf/baselines/pb-real-window.json",
    "crates/bitty-perf/baselines/pb-typical-session.json",
    "crates/bitty-perf/baselines/pb-latency.json",
    "crates/bitty-perf/baselines/pb-throughput-floor.json",
    "crates/bitty-perf/baselines/pb-idle.json",
    "crates/bitty-perf/baselines/parser-throughput.json",
    "crates/bitty-perf/baselines/pb-real-soak.json",
    "crates/bitty-perf/baselines/pb-dogfood-session.json",
];

#[test]
fn pin_record_names_every_budget_and_artifact() {
    let doc = std::fs::read_to_string(
        workspace_root().join("crates/bitty-perf/baselines/harness-pin.md"),
    )
    .expect("harness-pin.md must exist");
    for budget in ["PB-1", "PB-2", "PB-3", "PB-4", "PB-5", "PB-6", "PB-7"] {
        assert!(doc.contains(budget), "pin record must name {budget}");
    }
    for artifact in PINNED_ARTIFACTS {
        let file = artifact.rsplit('/').next().unwrap_or(artifact);
        assert!(
            doc.contains(file),
            "pin record must name pinned artifact {file}"
        );
    }
    assert!(
        doc.contains("OQ-100"),
        "pin record must state the OQ-100 gating block"
    );
    assert!(
        !doc.contains("/home/") && !doc.contains("/mnt/") && !doc.contains("C:\\Users"),
        "pin record must not embed a host path"
    );
}

#[test]
fn every_pinned_artifact_exists_with_schema_and_provenance() {
    for artifact in PINNED_ARTIFACTS {
        let path = workspace_root().join(artifact);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("pinned artifact {} must exist: {e}", path.display()));
        assert!(text.starts_with('{') && text.trim_end().ends_with('}'));
        for key in [
            "\"schema_version\"",
            "\"task\"",
            "\"captured_at\"",
            "\"revision\"",
        ] {
            assert!(
                text.contains(key),
                "pinned artifact {artifact} missing {key}"
            );
        }
        assert!(
            !text.contains("/home/") && !text.contains("/mnt/") && !text.contains("C:\\Users"),
            "pinned artifact {artifact} must not embed a host path"
        );
    }
}
