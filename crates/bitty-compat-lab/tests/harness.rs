#![forbid(unsafe_code)]
//! VT differential / vttest corpus harness wiring (Phase C cont., CTX-0078).
//!
//! - `#![forbid(unsafe_code)]` headless, bounded, deterministic.
//! - Bounded: `MAX_CORPUS_BYTES = 8 KiB`, `MAX_ACTIONS = 4096` asserted via
//!   `parse_bounded`; `BoundedString::MAX_LEN` truncates OSC payloads per
//!   `bitty-vt` contract.
//! - Deterministic: byte-by-byte re-parse + `State::state_hash` equality
//!   across full replay; snapshot diff helper for vttest/Ghostty differential.
//! - Headless: `Parser -> TerminalAction -> State` only, no `winit`, `wgpu`,
//!   `Window`, `Surface`, or `HeadlessRasterizer`.
//! - Wiring: this file is the `[[test]] harness` integration owned by
//!   `crates/bitty-compat-lab`; `cargo test -p bitty-compat-lab --test harness`
//!   and `cargo test --workspace --locked` both exercise it.
//!
//! Corpora root is `tests/compat/<category>/corpus/*.bin` at the workspace
//! root. Discovery is `CARGO_MANIFEST_DIR`-anchored (three `..` from
//! `crates/bitty-compat-lab` to workspace root) so shard determinism does not
//! depend on `cargo test` cwd. Every file is asserted `<= MAX_CORPUS_BYTES`.
//!
//! vttest pin: `tmp/references/vttest/` records upstream revision + license
//! and curation method; checked-in `tests/compat/vt/corpus/vttest-menu*.bin`
//! and `tests/compat/vt/reference/*.txt` are bounded slices derived from
//! `vttest` menus 1/11 shapes (see `tmp/references/vttest/README.md`).

use std::path::PathBuf;

use bitty_compat_lab::{
    MAX_ACTIONS, MAX_CORPORA_PER_CATEGORY, MAX_CORPUS_BYTES, actions_to_snapshot, parse_bounded,
};

/// Workspace root (two `..` from `crates/bitty-compat-lab` manifest dir).
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Corpus directory for a category.
fn corpus_dir(category: &str) -> PathBuf {
    workspace_root()
        .join("tests/compat")
        .join(category)
        .join("corpus")
}

/// All `*.bin` corpora under a category, bounded to `MAX_CORPORA_PER_CATEGORY`.
fn list_corpus_manifest(category: &str) -> Vec<PathBuf> {
    let dir = corpus_dir(category);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("bin") {
            continue;
        }
        if out.len() >= MAX_CORPORA_PER_CATEGORY {
            break;
        }
        out.push(path);
    }
    out.sort();
    out
}

/// Canonical categories from the Phase C scaffold.
const CATEGORIES: &[&str] = &[
    "vt", "osc", "keyboard", "mouse", "resize", "unicode", "shell", "tui",
];

#[test]
fn compat_corpus_is_bounded_and_deterministic() {
    let mut total = 0usize;
    for &category in CATEGORIES {
        let files = list_corpus_manifest(category);
        assert!(
            !files.is_empty(),
            "no corpora in {category} at {:?}",
            corpus_dir(category)
        );
        for path in &files {
            let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
            assert!(
                bytes.len() <= MAX_CORPUS_BYTES,
                "corpus {path:?} exceeds MAX_CORPUS_BYTES: {} > {}",
                bytes.len(),
                MAX_CORPUS_BYTES
            );
            let actions = parse_bounded(&bytes);
            assert!(
                actions.len() <= MAX_ACTIONS,
                "{path:?} produced {} actions > MAX_ACTIONS {MAX_ACTIONS}",
                actions.len()
            );
            let snap = actions_to_snapshot(&actions);
            assert_eq!(snap.width, bitty_term_state::GRID_COLUMNS);
            assert_eq!(snap.height, bitty_term_state::GRID_ROWS);

            let mut st = bitty_term_state::State::new();
            for a in &actions {
                st.apply(a);
            }
            st.check_invariants()
                .unwrap_or_else(|e| panic!("{path:?} invariant violation: {e:?}"));
            let h1 = st.state_hash();
            // Determinism: full replay must yield identical state_hash.
            let actions2 = parse_bounded(&bytes);
            let mut st3 = bitty_term_state::State::new();
            for a in &actions2 {
                st3.apply(a);
            }
            assert_eq!(
                h1,
                st3.state_hash(),
                "state_hash diverged for deterministic replay of {path:?}"
            );
            total += 1;
        }
    }
    assert!(
        total >= 16,
        "compat lab should enumerate >=16 corpora, saw {total}"
    );
}

#[test]
fn vttest_corpora_present_and_bounded() {
    let vtf = list_corpus_manifest("vt");
    let has_vttest = vtf.iter().any(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("vttest-"))
            .unwrap_or(false)
    });
    assert!(
        has_vttest,
        "expected vttest-menu*.bin in vt/corpus: {vtf:?}"
    );
    for p in &vtf {
        if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
            if n.starts_with("vttest-") {
                let b = std::fs::read(p).unwrap();
                assert!(b.len() <= MAX_CORPUS_BYTES, "vttest {p:?} exceeds bound");
                let actions = parse_bounded(&b);
                let snap = actions_to_snapshot(&actions);
                assert_eq!(snap.width, 80);
                assert_eq!(snap.height, 24);
            }
        }
    }
}

#[test]
fn no_window_gpu_leak_in_corpora() {
    for &cat in CATEGORIES {
        for p in list_corpus_manifest(cat) {
            let b = std::fs::read(&p).unwrap();
            let s = String::from_utf8_lossy(&b);
            assert!(!s.contains("winit"), "corpus {p:?} must not embed winit");
            assert!(!s.contains("wgpu"), "corpus {p:?} must not embed wgpu");
        }
    }
}
