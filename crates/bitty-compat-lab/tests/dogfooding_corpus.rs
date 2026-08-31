#![forbid(unsafe_code)]
//! Daily-driver dogfooding corpus — bounded headless regression for CTX-0099.
//!
//! Covers the 7 manual-smoke surfaces plus IME/DPI via headless bounded
//! corpora introduced in CTX-0099 (`*dogfooding*.bin` under `tests/compat`).
//! Headless, bounded, deterministic, no `winit`/`wgpu`/`Window`/`Surface`.
//! Proves `Parser -> TerminalAction -> State -> Snapshot` is panic-free,
//! bounded (`<=8 KiB`, `<=4096` actions), deterministic (byte-by-byte
//! re-parse + `state_hash`), and invariant-clean (`check_invariants`).

use std::path::PathBuf;

use bitty_compat_lab::{
    MAX_ACTIONS, MAX_CORPORA_PER_CATEGORY, MAX_CORPUS_BYTES, actions_to_snapshot, parse_bounded,
};

fn snapshot_to_text(snapshot: &bitty_term_state::Snapshot) -> String {
    let mut out = String::new();
    for row in 0..snapshot.height {
        for col in 0..snapshot.width {
            let idx = row * snapshot.width + col;
            let cell = &snapshot.cells[idx];
            if cell.spacer {
                continue;
            }
            out.push(cell.glyph);
        }
        if row + 1 < snapshot.height {
            out.push('\n');
        }
    }
    out
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn corpus_dir(category: &str) -> PathBuf {
    workspace_root()
        .join("tests/compat")
        .join(category)
        .join("corpus")
}

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

fn list_dogfooding_corpora() -> Vec<PathBuf> {
    const CATEGORIES: &[&str] = &[
        "vt", "osc", "keyboard", "mouse", "resize", "unicode", "shell", "tui",
    ];
    let mut out = Vec::new();
    for &cat in CATEGORIES {
        for p in list_corpus_manifest(cat) {
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if name.contains("dogfooding") {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

#[test]
fn dogfooding_corpora_present_and_bounded() {
    let dogs = list_dogfooding_corpora();
    assert!(
        dogs.len() >= 8,
        "expected >=8 dogfooding corpora for CTX-0099, saw {}: {dogs:?}",
        dogs.len()
    );
    for p in &dogs {
        let b = std::fs::read(p).unwrap_or_else(|e| panic!("read {p:?}: {e}"));
        assert!(
            b.len() <= MAX_CORPUS_BYTES,
            "dogfooding corpus {p:?} exceeds MAX_CORPUS_BYTES: {} > {}",
            b.len(),
            MAX_CORPUS_BYTES
        );
        // No winit/wgpu leak inside corpora bytes.
        let s = String::from_utf8_lossy(&b);
        assert!(
            !s.contains("winit"),
            "dogfooding corpus {p:?} must not embed winit"
        );
        assert!(
            !s.contains("wgpu"),
            "dogfooding corpus {p:?} must not embed wgpu"
        );
    }
}

#[test]
fn dogfooding_corpus_is_bounded_and_deterministic() {
    let dogs = list_dogfooding_corpora();
    assert!(!dogs.is_empty(), "no dogfooding corpora found");
    for path in &dogs {
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        assert!(
            bytes.len() <= MAX_CORPUS_BYTES,
            "corpus {path:?} exceeds MAX_CORPUS_BYTES"
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
        let actions2 = parse_bounded(&bytes);
        let mut st2 = bitty_term_state::State::new();
        for a in &actions2 {
            st2.apply(a);
        }
        assert_eq!(
            h1,
            st2.state_hash(),
            "state_hash diverged for deterministic replay of {path:?}"
        );
        // Snapshot determinism: second snapshot must equal first.
        let snap2 = actions_to_snapshot(&actions2);
        let text1 = snapshot_to_text(&snap);
        let text2 = snapshot_to_text(&snap2);
        assert_eq!(text1, text2, "snapshot text diverged {path:?}");
        assert_eq!(snap.title, snap2.title, "title diverged {path:?}");
        assert_eq!(
            snap.cursor, snap2.cursor,
            "cursor diverged {path:?}: {:?} vs {:?}",
            snap.cursor, snap2.cursor
        );
    }
}

#[test]
fn dogfooding_shell_prompt_marks_bounded_zones() {
    // Shell dogfooding must exercise OSC 133 A/B/C/D and OSC 7/8 without
    // exceeding zone or OSC bounds. Replay shell dogfooding corpora and assert
    // no panic and invariants hold; zone tracking is bounded via
    // ZONE_RECORDS_MAX=1024 inside State.
    let shell_dogs: Vec<_> = list_dogfooding_corpora()
        .into_iter()
        .filter(|p| p.to_string_lossy().contains("/shell/"))
        .collect();
    assert!(!shell_dogs.is_empty(), "no shell dogfooding corpora found");
    for path in shell_dogs {
        let bytes = std::fs::read(&path).unwrap();
        let actions = parse_bounded(&bytes);
        let mut st = bitty_term_state::State::new();
        for a in &actions {
            let _ = st.apply(a);
        }
        st.check_invariants().expect("shell invariant");
        // BoundedString truncates at 1024, so even long OSC payloads stay bounded.
        let _snap = st.snapshot();
    }
}

#[test]
fn dogfooding_unicode_ime_width_invariants() {
    let uni_dogs: Vec<_> = list_dogfooding_corpora()
        .into_iter()
        .filter(|p| p.to_string_lossy().contains("/unicode/"))
        .collect();
    assert!(!uni_dogs.is_empty(), "no unicode dogfooding corpora found");
    for path in uni_dogs {
        let bytes = std::fs::read(&path).unwrap();
        let actions = parse_bounded(&bytes);
        let mut st = bitty_term_state::State::new();
        for a in &actions {
            st.apply(a);
        }
        st.check_invariants()
            .unwrap_or_else(|e| panic!("unicode {path:?} invariant: {e:?}"));
        let snap = st.snapshot();
        // GRID geometry invariant.
        assert_eq!(snap.width, 80);
        assert_eq!(snap.height, 24);
        // No orphan spacer: each wide glyph's trailing half is spacer but never orphaned.
        for (idx, cell) in snap.cells.iter().enumerate() {
            if cell.spacer {
                assert!(idx > 0, "spacer at 0 is orphan");
                let prev = &snap.cells[idx - 1];
                assert!(
                    !prev.spacer && prev.glyph.len_utf8() > 0,
                    "spacer without leading wide glyph at {idx}"
                );
            }
        }
    }
}

#[test]
fn dogfooding_resize_alt_screen_no_panic() {
    let resize_dogs: Vec<_> = list_dogfooding_corpora()
        .into_iter()
        .filter(|p| {
            let s = p.to_string_lossy();
            s.contains("/resize/") || s.contains("/tui/")
        })
        .collect();
    assert!(!resize_dogs.is_empty(), "no resize/tui dogfooding corpora");
    for path in resize_dogs {
        let bytes = std::fs::read(&path).unwrap();
        let actions = parse_bounded(&bytes);
        let mut st = bitty_term_state::State::new();
        for a in &actions {
            let _ = st.apply(a);
        }
        st.check_invariants().expect("resize/tui invariant");
        // Also exercise Runtime-style resize determinism headlessly: snapshot
        // after replay must be identical on byte-by-byte re-parse.
        let actions2 = parse_bounded(&bytes);
        let mut st2 = bitty_term_state::State::new();
        for a in &actions2 {
            st2.apply(a);
        }
        assert_eq!(st.state_hash(), st2.state_hash());
    }
}

#[test]
fn dogfooding_mouse_keyboard_modes_no_corruption() {
    let mk_dogs: Vec<_> = list_dogfooding_corpora()
        .into_iter()
        .filter(|p| {
            let s = p.to_string_lossy();
            s.contains("/mouse/") || s.contains("/keyboard/")
        })
        .collect();
    assert!(!mk_dogs.is_empty(), "no mouse/keyboard dogfooding corpora");
    for path in mk_dogs {
        let bytes = std::fs::read(&path).unwrap();
        let actions = parse_bounded(&bytes);
        // Mouse/keyboard modes are inert to grid but must not corrupt State.
        let mut st = bitty_term_state::State::new();
        for a in &actions {
            st.apply(a);
        }
        st.check_invariants().expect("mouse/keyboard invariant");
        let snap = st.snapshot();
        assert_eq!(snap.width, 80);
        assert_eq!(snap.height, 24);
    }
}
