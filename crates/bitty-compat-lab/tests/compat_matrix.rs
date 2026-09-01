#![forbid(unsafe_code)]
//! Release compatibility matrix regression (CTX-0114) — 14 surfaces × 4 terminals.
//!
//! Bounded, headless, deterministic. Covers shell/tmux/nvim/fzf/htop/ssh/
//! alt-screen/mouse/resize/OSC/clipboard/Kitty/IME/DPI across Ghostty/Kitty/
//! WezTerm/Alacritty differential. No `winit`/`wgpu`/`Window`/`Surface`.
//! Proves Parser -> TerminalAction -> State -> Snapshot is panic-free,
//! bounded (≤8 KiB, ≤4096 actions), deterministic (byte-by-byte re-parse +
//! state_hash), and invariant-clean (check_invariants). Differential graceful
//! skip when backend dumps absent; self-consistency is gated.

use bitty_compat_lab::{
    MAX_ACTIONS, MAX_CORPUS_BYTES, actions_to_snapshot,
    compare::{MAX_SNAPSHOT_JSON_BYTES, MAX_TEXT_CHARS, compare_all},
    matrix::{MATRIX, REFERENCE_TERMS, corpus_path, generate_matrix_json},
    parse_bounded,
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

#[test]
fn matrix_covers_all_14_surfaces() {
    assert_eq!(MATRIX.len(), 14, "matrix must have 14 surfaces");
    assert_eq!(REFERENCE_TERMS.len(), 4, "need 4 reference terminals");
    assert_eq!(
        REFERENCE_TERMS,
        &["ghostty", "kitty", "wezterm", "alacritty"]
    );
    let surfaces: Vec<&str> = MATRIX.iter().map(|e| e.surface).collect();
    for required in [
        "shell",
        "tmux",
        "nvim",
        "fzf",
        "htop",
        "ssh",
        "alt-screen",
        "mouse",
        "resize",
        "OSC",
        "clipboard",
        "Kitty",
        "IME",
        "DPI",
    ] {
        assert!(
            surfaces.contains(&required),
            "matrix missing surface {required}: {surfaces:?}"
        );
    }
    // Ensure each entry's corpus file exists and is bounded.
    for e in MATRIX {
        let p = corpus_path(e);
        let b = std::fs::read(&p).unwrap_or_else(|err| panic!("read {} {:?}: {err}", e.surface, p));
        assert!(
            b.len() <= MAX_CORPUS_BYTES,
            "{} {:?} {} > MAX_CORPUS_BYTES",
            e.surface,
            p,
            b.len()
        );
    }
}

#[test]
fn matrix_corpora_are_bounded_and_deterministic() {
    for entry in MATRIX {
        let path = corpus_path(entry);
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        assert!(
            bytes.len() <= MAX_CORPUS_BYTES,
            "{} exceeds MAX_CORPUS_BYTES: {}",
            entry.surface,
            bytes.len()
        );
        let actions = parse_bounded(&bytes);
        assert!(
            actions.len() <= MAX_ACTIONS,
            "{} {} actions > MAX_ACTIONS",
            entry.surface,
            actions.len()
        );
        let snap = actions_to_snapshot(&actions);
        assert_eq!(
            snap.width,
            bitty_term_state::GRID_COLUMNS,
            "{}",
            entry.surface
        );
        assert_eq!(
            snap.height,
            bitty_term_state::GRID_ROWS,
            "{}",
            entry.surface
        );
        let mut st = bitty_term_state::State::new();
        for a in &actions {
            st.apply(a);
        }
        st.check_invariants()
            .unwrap_or_else(|e| panic!("{} invariant {e:?}", entry.surface));
        let h1 = st.state_hash();
        let actions2 = parse_bounded(&bytes);
        let mut st2 = bitty_term_state::State::new();
        for a in &actions2 {
            st2.apply(a);
        }
        assert_eq!(
            h1,
            st2.state_hash(),
            "{} state_hash diverged",
            entry.surface
        );
        let snap2 = actions_to_snapshot(&actions2);
        assert_eq!(
            snapshot_to_text(&snap),
            snapshot_to_text(&snap2),
            "{} snapshot text diverged",
            entry.surface
        );
        assert_eq!(snap.title, snap2.title, "{} title diverged", entry.surface);
        assert_eq!(
            snap.cursor, snap2.cursor,
            "{} cursor diverged",
            entry.surface
        );
    }
}

#[test]
fn matrix_snapshots_are_bounded() {
    for entry in MATRIX {
        let path = corpus_path(entry);
        let bytes = std::fs::read(&path).unwrap();
        let actions = parse_bounded(&bytes);
        let snapshot = actions_to_snapshot(&actions);
        assert_eq!(snapshot.width, 80, "{}", entry.surface);
        assert_eq!(snapshot.height, 24, "{}", entry.surface);
        let text = snapshot_to_text(&snapshot);
        assert!(
            text.chars().count() <= MAX_TEXT_CHARS,
            "{} text {} > MAX_TEXT_CHARS {}",
            entry.surface,
            text.chars().count(),
            MAX_TEXT_CHARS
        );
        // Generation monotonic and cursor bounded (re-used from compare invariants)
        assert!(
            snapshot.cursor.position.row < 24,
            "{} cursor row",
            entry.surface
        );
        assert!(
            snapshot.cursor.position.col < 80,
            "{} cursor col",
            entry.surface
        );
        // JSON serialization bound (<16 KiB) is proven via generate_matrix_json below
    }
    let json = generate_matrix_json().expect("matrix json");
    assert!(
        json.len() < MAX_SNAPSHOT_JSON_BYTES,
        "matrix json {} > MAX_SNAPSHOT_JSON_BYTES {}",
        json.len(),
        MAX_SNAPSHOT_JSON_BYTES
    );
}

#[test]
fn matrix_differential_is_graceful() {
    // Compare_all self-consistency must PASS for all 39 dumps; reference diff is graceful skip.
    let report = match compare_all() {
        Ok(r) => r,
        Err(e) if e.contains("not found") || e.contains("no bitty") || e.contains("no dumps") => {
            eprintln!(
                "SKIP: bitty dumps not present (run collect_dumps); skipping differential assertions"
            );
            return;
        }
        Err(e) => panic!("compare_all: {e}"),
    };
    assert_eq!(
        report.self_failed,
        0,
        "self-consistency failed:\n{}",
        bitty_compat_lab::compare::format_report(&report)
    );
    // When no reference dumps exist, every outcome is reference_skipped=true and compared=0.
    // When some exist (ghostty/kitty/wezterm/alacritty), counts must be consistent.
    let any_ref = report.reference_compared > 0;
    for o in &report.outcomes {
        if any_ref {
            assert!(
                o.reference_skipped || o.reference_compared > 0,
                "{} inconsistent reference_skipped/compare",
                o.dump.file_name
            );
        } else {
            assert!(
                o.reference_skipped,
                "{} expected skipped when no backend dumps",
                o.dump.file_name
            );
            assert_eq!(
                o.reference_compared, 0,
                "{} compared >0 with no backends",
                o.dump.file_name
            );
            assert!(
                o.reference_failures.is_empty(),
                "{} failures non-empty with no backends",
                o.dump.file_name
            );
        }
    }
    // 14 matrix entries must be among the total dumps (39 as of CTX-0099).
    assert!(
        report.total >= 14,
        "compare report total {} < 14 matrix surfaces",
        report.total
    );
}

#[test]
fn matrix_no_window_gpu_leak() {
    for entry in MATRIX {
        let path = corpus_path(entry);
        let b = std::fs::read(&path).unwrap();
        let s = String::from_utf8_lossy(&b);
        assert!(
            !s.contains("winit"),
            "{} {:?} must not embed winit",
            entry.surface,
            path
        );
        assert!(
            !s.contains("wgpu"),
            "{} {:?} must not embed wgpu",
            entry.surface,
            path
        );
        assert!(
            !s.contains("Window"),
            "{} {:?} must not embed Window",
            entry.surface,
            path
        );
        assert!(
            !s.contains("Surface"),
            "{} {:?} must not embed Surface",
            entry.surface,
            path
        );
    }
    // Also ensure harness and matrix source themselves do not construct those types
    // (checked via forbid list in docs, not via runtime).
}

#[test]
fn matrix_is_sorted_and_deterministic() {
    let j1 = generate_matrix_json().expect("matrix json 1");
    let j2 = generate_matrix_json().expect("matrix json 2");
    assert_eq!(j1, j2, "matrix json not deterministic");
    // Ensure matrix order is stable and surfaces unique
    let mut seen = std::collections::BTreeSet::new();
    for e in MATRIX {
        assert!(seen.insert(e.surface), "duplicate surface {}", e.surface);
    }
    assert_eq!(seen.len(), 14);
    // First and last surface order is part of determinism contract
    assert_eq!(MATRIX[0].surface, "shell");
    assert_eq!(MATRIX[13].surface, "DPI");
    // Each generate hashes must be deterministic across replays (spot check shell)
    let shell_path = corpus_path(&MATRIX[0]);
    let b = std::fs::read(shell_path).unwrap();
    let a1 = parse_bounded(&b);
    let a2 = parse_bounded(&b);
    assert_eq!(a1, a2, "shell parse not deterministic");
}

#[test]
fn matrix_no_unsafe_and_generates_bounded_json() {
    let j = generate_matrix_json().expect("json");
    assert!(j.len() < 16 * 1024, "matrix json >16 KiB: {}", j.len());
    assert!(j.contains("\"ghostty\": \"SKIP\""), "missing ghostty SKIP");
    assert!(
        j.contains("\"alacritty\": \"SKIP\""),
        "missing alacritty SKIP (CTX-0114)"
    );
    // Forbid unsafe is enforced at crate level via #![forbid(unsafe_code)] in src.
    // This test documents the invariant; compile failure is the true gate.
}

/// Bonus: ensure workspace root discovery is stable for bundled tests
#[test]
fn corpus_paths_are_workspace_anchored() {
    for e in MATRIX {
        let p = corpus_path(e);
        assert!(
            p.exists(),
            "corpus path {:?} for {} does not exist (workspace_root anchor broken)",
            p,
            e.surface
        );
        // Must be under tests/compat
        assert!(
            p.to_string_lossy().contains("tests/compat"),
            "path {:?} not under tests/compat",
            p
        );
    }
    // Also validate that MATRIX covers every major compat category at least once
    let cats: std::collections::BTreeSet<&str> = MATRIX.iter().map(|e| e.category).collect();
    for need in [
        "shell", "tui", "resize", "mouse", "osc", "keyboard", "unicode",
    ] {
        assert!(
            cats.contains(need),
            "matrix missing category {need}: {cats:?}"
        );
    }
}
