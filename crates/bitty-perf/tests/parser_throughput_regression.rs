//! Parser-throughput regression gate (CTX-0576, M1-11).
//!
//! CI half of the compatibility-milestone "Performance guardrail": it measures
//! `bitty-vt::Parser::advance` over the bounded representative corpora and
//! compares the escape/plain throughput ratios against the committed baseline
//! in `crates/bitty-perf/baselines/parser-throughput.json`.
//!
//! Deterministic and bounded (`CI_SAMPLE_BYTES` 512 KiB per corpus,
//! `CI_ROUNDS` 3), headless, `#![forbid(unsafe_code)]`, no clock/RNG/network/
//! display/host path. It runs under plain `cargo test`, so it participates in
//! both `just check` and the `Quality gates` CI job without requiring a
//! release build; the absolute MB/s floor is only enforced for optimized
//! builds (see `bitty_perf::parser_throughput::regression_check`).

#![forbid(unsafe_code)]

use bitty_perf::parser_throughput::{
    CORPUS_SPECS, MAX_SEGMENT_BYTES, PARSER_BASELINE_REL_PATH, PLAIN_TEXT_ID, REGRESSION_FACTOR,
    ci_report, committed_baseline, load_segments, regression_check, workspace_root,
};

#[test]
fn parser_throughput_meets_the_committed_ratio_gate() {
    let report = ci_report().expect("bounded CI measurement must run");
    let baseline = committed_baseline().expect("committed baseline must parse");

    // Every escape corpus must be measured, and the reference must exceed zero.
    assert_eq!(report.corpora.len(), CORPUS_SPECS.len());
    assert!(report.mb_s(PLAIN_TEXT_ID).is_some_and(|v| v > 0.0));

    let regression = regression_check(&report, &baseline);
    assert!(
        regression.all_passed(),
        "parser throughput regressed versus committed baseline:\n{}",
        regression.format_summary()
    );
}

#[test]
fn committed_baseline_is_provenanced_and_positive() {
    let baseline = committed_baseline().expect("committed baseline must parse");
    let ids: Vec<&str> = baseline.corpora.iter().map(|c| c.id.as_str()).collect();
    let expected: Vec<&str> = CORPUS_SPECS.iter().map(|s| s.id).collect();
    assert_eq!(
        ids, expected,
        "baseline corpus roster must match the module"
    );
    for corpus in &baseline.corpora {
        assert!(
            corpus.mb_s.is_finite() && corpus.mb_s > 0.0,
            "baseline {} must be a positive finite number, saw {}",
            corpus.id,
            corpus.mb_s
        );
    }
    assert_eq!(baseline.regression_factor, REGRESSION_FACTOR);

    // Provenance is committed alongside the numbers so the baseline is
    // reproducible rather than an unattributed figure.
    let root = workspace_root();
    let text = std::fs::read_to_string(root.join(PARSER_BASELINE_REL_PATH))
        .expect("baseline artifact must be committed");
    for key in [
        "\"task\"",
        "\"issue\"",
        "\"revision\"",
        "\"command\"",
        "\"machine_class\"",
        "\"captured_at\"",
    ] {
        assert!(
            text.contains(key),
            "baseline artifact missing provenance key {key}"
        );
    }
    assert!(
        !text.contains("pending") && !text.contains("unspecified"),
        "baseline artifact still carries placeholder provenance"
    );
}

#[test]
fn corpora_are_reused_bounded_and_deterministic() {
    let first = load_segments().expect("segments must load");
    let second = load_segments().expect("segments must load");
    assert_eq!(first.len(), CORPUS_SPECS.len());
    for ((spec, a), (_, b)) in first.iter().zip(second.iter()) {
        assert_eq!(a, b, "corpus {} must be byte-identical run to run", spec.id);
        assert!(!a.is_empty(), "corpus {} must not be empty", spec.id);
        assert!(
            a.len() <= MAX_SEGMENT_BYTES,
            "corpus {} exceeds MAX_SEGMENT_BYTES",
            spec.id
        );
    }
}

#[test]
fn pathological_regression_is_caught() {
    // Synthetic proof that the gate is not vacuous: a report where every escape
    // corpus collapses to the floor must fail against the committed baseline.
    let baseline = committed_baseline().expect("committed baseline must parse");
    let mut report = ci_report().expect("bounded CI measurement must run");
    for corpus in report.corpora.iter_mut() {
        if corpus.id != PLAIN_TEXT_ID {
            corpus.mb_s = 0.5;
        }
    }
    let regression = regression_check(&report, &baseline);
    assert!(
        !regression.all_passed(),
        "a collapsed escape corpus must fail the ratio gate"
    );
}
