#![forbid(unsafe_code)]
//! Differential comparator — bounded, deterministic, headless.
//!
//! Exercises `bitty-compat-lab::compare` over the 30 bitty dumps produced by
//! `collect_dumps` (CTX-0086). Self-consistency is asserted for every dump:
//! replay of `tests/compat/<category>/corpus/*.bin` must reproduce the
//! stored `state_hash`, `Snapshot` grid/cursor/title/generation, `text`,
//! `bytes_len`/`actions_len`, and `State` invariants plus `damage_since`
//! sanity. Reference backends (ghostty/kitty/wezterm) are diffed opaquely
//! when present under `tmp/references/<backend>/*.snapshot.json` — graceful
//! skip when absent (no network, no `winit`/`wgpu`/`Window`/`Surface`).

use bitty_compat_lab::compare::{
    MAX_ACTIONS, MAX_CORPUS_BYTES, MAX_SNAPSHOT_JSON_BYTES, MAX_SNAPSHOTS, MAX_TEXT_CHARS,
    compare_all, format_report, load_bitty_dumps,
};

#[test]
fn comparator_is_bounded_and_headless() {
    // No winit/wgpu/window/surface leak — source-level invariant by crate policy,
    // enforced via `forbid(unsafe)` and by never constructing such types here.
    // Bounds are pinned constants re-exported by the comparator.
    assert_eq!(MAX_CORPUS_BYTES, 8 * 1024);
    assert_eq!(MAX_ACTIONS, 4096);
    assert_eq!(MAX_SNAPSHOT_JSON_BYTES, 16 * 1024);
    assert_eq!(MAX_SNAPSHOTS, 64);
    assert_eq!(MAX_TEXT_CHARS, 80 * 24 + 24);
}

#[test]
fn load_bitty_dumps_is_bounded_and_sorted() {
    let dumps = load_bitty_dumps().expect("load_bitty_dumps");
    assert!(
        !dumps.is_empty(),
        "expected bitty dumps at tmp/references/bitty"
    );
    assert!(
        dumps.len() <= MAX_SNAPSHOTS,
        "dumps {} > MAX_SNAPSHOTS {}",
        dumps.len(),
        MAX_SNAPSHOTS
    );
    // Determinism: file names sorted.
    let mut sorted = dumps.clone();
    sorted.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    assert_eq!(
        dumps.iter().map(|d| &d.file_name).collect::<Vec<_>>(),
        sorted.iter().map(|d| &d.file_name).collect::<Vec<_>>(),
        "load_bitty_dumps must return sorted file_name order"
    );
    for d in &dumps {
        assert!(
            d.bytes_len <= MAX_CORPUS_BYTES,
            "{} bytes_len {} > MAX_CORPUS_BYTES",
            d.file_name,
            d.bytes_len
        );
        assert!(
            d.actions_len <= MAX_ACTIONS,
            "{} actions_len {} > MAX_ACTIONS",
            d.file_name,
            d.actions_len
        );
        assert!(
            d.text.chars().count() <= MAX_TEXT_CHARS,
            "{} text chars {} > MAX_TEXT_CHARS",
            d.file_name,
            d.text.chars().count()
        );
        assert_eq!(d.width, 80, "{} width", d.file_name);
        assert_eq!(d.height, 24, "{} height", d.file_name);
        assert_eq!(d.state_hash_version, 1, "{} hash version", d.file_name);
    }
    // 30 dumps as of CTX-0086; guard against regressions accidentally dropping fixtures.
    assert!(
        dumps.len() >= 30,
        "expected >=30 bitty dumps (CTX-0086), saw {}",
        dumps.len()
    );
}

#[test]
fn comparator_is_deterministic_and_self_consistent() {
    let first = compare_all().expect("compare_all first run");
    let second = compare_all().expect("compare_all second run");
    assert_eq!(first.total, second.total, "determinism: total diverged");
    assert_eq!(
        first.self_passed, second.self_passed,
        "determinism: self_passed diverged"
    );
    assert_eq!(
        first.self_failed, second.self_failed,
        "determinism: self_failed diverged"
    );
    // Outcomes must be identical: same file_name order, same self_consistent, same failures.
    assert_eq!(first.outcomes.len(), second.outcomes.len());
    for (a, b) in first.outcomes.iter().zip(second.outcomes.iter()) {
        assert_eq!(a.dump.file_name, b.dump.file_name);
        assert_eq!(a.self_consistent, b.self_consistent);
        assert_eq!(a.self_failure, b.self_failure);
        assert_eq!(a.reference_compared, b.reference_compared);
        assert_eq!(a.reference_failures, b.reference_failures);
    }
    // All self-consistency must pass — bitty dumps were produced headlessly from
    // the same corpora, so any failure is a contract violation.
    assert_eq!(
        first.self_failed,
        0,
        "self-consistency failed:\n{}",
        format_report(&first)
    );
    // Storage and runtime invariants: generation bounds, cursor bounds.
    for o in &first.outcomes {
        assert!(
            o.dump.cursor_row < 24,
            "{} cursor_row {} >= 24",
            o.dump.file_name,
            o.dump.cursor_row
        );
        assert!(
            o.dump.cursor_col < 80,
            "{} cursor_col {} >= 80",
            o.dump.file_name,
            o.dump.cursor_col
        );
    }
}

#[test]
fn comparator_no_unbounded_heap() {
    // Cheap heap bound: re-run under a fresh allocation scope and assert no
    // outcome contains an excessively large string (text bounded, report bounded).
    let report = compare_all().expect("compare_all");
    let rendered = format_report(&report);
    // 64 outcomes * at most a short line each when passing; failures are bounded
    // by single-line reasons. A healthy report stays well under 1 MiB.
    assert!(
        rendered.len() < 1024 * 1024,
        "report unexpectedly large: {}",
        rendered.len()
    );
    for o in &report.outcomes {
        assert!(
            o.dump.text.len() < 64 * 1024,
            "{} text len {} exceeds safety cap",
            o.dump.file_name,
            o.dump.text.len()
        );
        if let Some(reason) = &o.self_failure {
            assert!(
                reason.len() < 8 * 1024,
                "{} self_failure reason {} exceeds safety cap",
                o.dump.file_name,
                reason.len()
            );
        }
        for f in &o.reference_failures {
            assert!(
                f.len() < 8 * 1024,
                "reference failure {f:?} exceeds safety cap"
            );
        }
    }
}

#[test]
fn comparator_reference_graceful_skip_when_absent() {
    // Reference backends (ghostty/kitty/wezterm) may have no per-corpus dumps.
    // The comparator must not fail or invent mismatches when they are absent.
    let report = compare_all().expect("compare_all");
    // When no reference dumps exist, every outcome is `reference_skipped==true`
    // and `reference_compared==0`. When some do exist, counts must be consistent.
    let any_ref = report.reference_compared > 0;
    for o in &report.outcomes {
        if any_ref {
            assert!(
                o.reference_skipped || o.reference_compared > 0,
                "{} inconsistent reference_skipped/reference_compared",
                o.dump.file_name
            );
        } else {
            assert!(
                o.reference_skipped,
                "{} expected reference_skipped when no backend dumps present",
                o.dump.file_name
            );
            assert_eq!(
                o.reference_compared, 0,
                "{} reference_compared >0 with no backends",
                o.dump.file_name
            );
            assert!(
                o.reference_failures.is_empty(),
                "{} reference_failures non-empty with no backends",
                o.dump.file_name
            );
        }
    }
}
